use lightbulb::{daemon, os, paths};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

async fn connect(sock: &std::path::Path) -> (Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf) {
    for _ in 0..50 {
        if let Ok(s) = UnixStream::connect(sock).await {
            let (r, w) = s.into_split();
            return (BufReader::new(r).lines(), w);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("daemon socket never came up");
}

async fn call(
    l: &mut Lines<BufReader<OwnedReadHalf>>,
    w: &mut OwnedWriteHalf,
    req: Value,
) -> Value {
    w.write_all(format!("{req}\n").as_bytes()).await.unwrap();
    serde_json::from_str(&l.next_line().await.unwrap().unwrap()).unwrap()
}

/// The tests here set a process-wide env var so the daemon never reads the real ~/.claude.
#[tokio::test]
async fn send_wakes_a_waiting_inbox_and_the_receipt_walks_to_delivered() {
    let dir = tempfile::tempdir().unwrap();
    let claude_home = tempfile::tempdir().unwrap();
    std::env::set_var("LIGHTBULB_CONNECT_CLAUDE_HOME", claude_home.path());
    let home = dir.path().to_path_buf();
    tokio::spawn(async move { daemon::run(&home).await.unwrap() });
    let sock = dir.path().join(paths::SOCKET);
    let (mut l, mut w) = connect(&sock).await;

    let birth = os::process(std::process::id()).unwrap().birth_micros;
    let reg = call(
        &mut l,
        &mut w,
        json!({"id": 1, "op": "register", "session": {
        "provider": "codex", "conversation": "b", "pid": std::process::id(),
        "birth_micros": birth, "cwd": "/tmp"}}),
    )
    .await;
    let to = reg["key"].as_str().unwrap().to_string();

    // A second connection waits on the inbox before anything is sent.
    let (mut l2, mut w2) = connect(&sock).await;
    let key = to.clone();
    let waiter = tokio::spawn(async move {
        call(
            &mut l2,
            &mut w2,
            json!({"id": 1, "op": "inbox", "key": key, "wait_secs": 30}),
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let sent = call(
        &mut l,
        &mut w,
        json!({"id": 2, "op": "send", "from": "x/codex:a",
        "to": to, "text": "hello", "request_id": "r1"}),
    )
    .await;
    assert_eq!(sent["ok"], true);
    assert_eq!(sent["receipt"]["state"], "stored");
    assert_eq!(sent["receipt"]["where"], "recipient");

    let got = waiter.await.unwrap();
    assert_eq!(got["ok"], true);
    assert_eq!(got["messages"][0]["text"], "hello");
    assert_eq!(got["messages"][0]["from"], "x/codex:a");

    let r = call(
        &mut l,
        &mut w,
        json!({"id": 3, "op": "receipt", "from": "x/codex:a", "request_id": "r1"}),
    )
    .await;
    assert_eq!(r["receipt"]["state"], "delivered");
    let r = call(
        &mut l,
        &mut w,
        json!({"id": 4, "op": "receipt", "from": "x/codex:a", "request_id": "nope"}),
    )
    .await;
    assert_eq!(r["error"], "not_found");
    let r = call(
        &mut l,
        &mut w,
        json!({"id": 5, "op": "inbox", "key": to, "wait_secs": 601}),
    )
    .await;
    assert_eq!(r["error"], "bad_request");
    let r = call(
        &mut l,
        &mut w,
        json!({"id": 6, "op": "inbox", "key": to, "wait_secs": 0}),
    )
    .await;
    assert_eq!(r["messages"], json!([]));
    call(&mut l, &mut w, json!({"id": 7, "op": "shutdown"})).await;
}

#[tokio::test]
async fn shutdown_ends_a_waiting_inbox_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let claude_home = tempfile::tempdir().unwrap();
    std::env::set_var("LIGHTBULB_CONNECT_CLAUDE_HOME", claude_home.path());
    let home = dir.path().to_path_buf();
    tokio::spawn(async move { daemon::run(&home).await.unwrap() });
    let sock = dir.path().join(paths::SOCKET);
    let (mut l, mut w) = connect(&sock).await;
    let waiter = tokio::spawn(async move {
        call(
            &mut l,
            &mut w,
            json!({"id": 1, "op": "inbox", "key": "x/codex:z", "wait_secs": 60}),
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let (mut l2, mut w2) = connect(&sock).await;
    call(&mut l2, &mut w2, json!({"id": 2, "op": "shutdown"})).await;
    let got = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
        .await
        .expect("inbox kept waiting through shutdown")
        .unwrap();
    assert_eq!(
        (got["ok"].clone(), got["messages"].clone()),
        (json!(true), json!([]))
    );
}

#[tokio::test]
async fn mail_arriving_after_a_waiting_inbox_hung_up_stays_in_the_mailbox() {
    let dir = tempfile::tempdir().unwrap();
    let claude_home = tempfile::tempdir().unwrap();
    std::env::set_var("LIGHTBULB_CONNECT_CLAUDE_HOME", claude_home.path());
    let home = dir.path().to_path_buf();
    tokio::spawn(async move { daemon::run(&home).await.unwrap() });
    let sock = dir.path().join(paths::SOCKET);
    let (mut l, mut w) = connect(&sock).await;
    // An address here: mail under another member's prefix goes to the outbox.
    let short = call(&mut l, &mut w, json!({"id": 1, "op": "status"})).await["member_key_short"]
        .as_str()
        .unwrap()
        .to_string();
    let key = format!("{short}/codex:gone-reader");

    // A reader waits, then hangs up before anything arrives.
    let (l2, mut w2) = connect(&sock).await;
    let req = json!({"id": 1, "op": "inbox", "key": key, "wait_secs": 30});
    w2.write_all(format!("{req}\n").as_bytes()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    drop((l2, w2));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let sent = call(
        &mut l,
        &mut w,
        json!({"id": 2, "op": "send", "from": "x/claude:a", "to": key, "text": "kept",
            "request_id": "r1"}),
    )
    .await;
    assert_eq!(sent["receipt"]["state"], "stored", "{sent}");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let (mut l3, mut w3) = connect(&sock).await;
    let got = call(
        &mut l3,
        &mut w3,
        json!({"id": 3, "op": "inbox", "key": key}),
    )
    .await;
    assert_eq!(got["messages"][0]["text"], "kept", "{got}");
    call(&mut l, &mut w, json!({"id": 4, "op": "shutdown"})).await;
}

#[tokio::test]
async fn send_qualifies_a_bare_name_and_refuses_bad_addresses() {
    let dir = tempfile::tempdir().unwrap();
    let claude_home = tempfile::tempdir().unwrap();
    std::env::set_var("LIGHTBULB_CONNECT_CLAUDE_HOME", claude_home.path());
    let home = dir.path().to_path_buf();
    tokio::spawn(async move { daemon::run(&home).await.unwrap() });
    let (mut l, mut w) = connect(&dir.path().join(paths::SOCKET)).await;
    let short = call(&mut l, &mut w, json!({"id": 1, "op": "status"})).await["member_key_short"]
        .as_str()
        .unwrap()
        .to_string();
    let send = |to: &str, rid: &str| {
        json!({"id": 2, "op": "send", "from": "x/claude:a", "to": to, "text": "hi",
            "request_id": rid})
    };
    let r = call(&mut l, &mut w, send("desk", "n1")).await;
    assert_eq!(r["receipt"]["state"], "stored", "{r}");
    let got = call(
        &mut l,
        &mut w,
        json!({"id": 3, "op": "inbox", "key": format!("{short}/desk")}),
    )
    .await;
    assert_eq!(got["messages"][0]["to"], format!("{short}/desk"), "{got}");
    for (i, bad) in [
        "*",
        &format!("{short}/*"),
        "a b",
        "x/",
        "/desk",
        "x/nope:c",
        "x/codex:",
        "x/codex:a b",
        "x/y/z",
    ]
    .iter()
    .enumerate()
    {
        let r = call(&mut l, &mut w, send(bad, &format!("b{i}"))).await;
        assert_eq!(
            (r["error"].as_str(), r["reason"].as_str()),
            (Some("bad_request"), Some("bad_address")),
            "{bad}: {r}"
        );
    }
    call(&mut l, &mut w, json!({"id": 4, "op": "shutdown"})).await;
}

#[tokio::test]
async fn a_peer_hanging_up_as_mail_arrives_never_loses_the_message() {
    let dir = tempfile::tempdir().unwrap();
    let claude_home = tempfile::tempdir().unwrap();
    std::env::set_var("LIGHTBULB_CONNECT_CLAUDE_HOME", claude_home.path());
    let home = dir.path().to_path_buf();
    tokio::spawn(async move { daemon::run(&home).await.unwrap() });
    let sock = dir.path().join(paths::SOCKET);
    let (mut l, mut w) = connect(&sock).await;
    let short = call(&mut l, &mut w, json!({"id": 1, "op": "status"})).await["member_key_short"]
        .as_str()
        .unwrap()
        .to_string();
    for i in 0..50 {
        let key = format!("{short}/codex:race{i}");
        let (r2, mut w2) = connect(&sock).await;
        let req = json!({"id": 1, "op": "inbox", "key": key, "wait_secs": 30});
        w2.write_all(format!("{req}\n").as_bytes()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // Send and hang up at the same instant: both reach the daemon in one go.
        let send = json!({"id": 2, "op": "send", "from": "x/claude:a", "to": key,
            "text": "kept", "request_id": format!("r{i}")});
        w.write_all(format!("{send}\n").as_bytes()).await.unwrap();
        drop((r2, w2));
        let sent: Value = serde_json::from_str(&l.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(sent["receipt"]["state"], "stored", "{sent}");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let got = call(&mut l, &mut w, json!({"id": 3, "op": "inbox", "key": key})).await;
        assert_eq!(got["messages"][0]["text"], "kept", "iteration {i}: {got}");
    }
    call(&mut l, &mut w, json!({"id": 4, "op": "shutdown"})).await;
}

#[tokio::test]
async fn an_assignment_is_delivered_with_its_fields_and_its_reply_is_its_result() {
    let dir = tempfile::tempdir().unwrap();
    let claude_home = tempfile::tempdir().unwrap();
    std::env::set_var("LIGHTBULB_CONNECT_CLAUDE_HOME", claude_home.path());
    let home = dir.path().to_path_buf();
    tokio::spawn(async move { daemon::run(&home).await.unwrap() });
    let sock = dir.path().join(paths::SOCKET);
    let (mut l, mut w) = connect(&sock).await;
    let birth = os::process(std::process::id()).unwrap().birth_micros;
    let mut keys = vec![];
    for (i, (provider, conv)) in [("codex", "a"), ("claude", "b")].into_iter().enumerate() {
        let reg = call(
            &mut l,
            &mut w,
            json!({"id": i + 1, "op": "register", "session": {
            "provider": provider, "conversation": conv, "pid": std::process::id(),
            "birth_micros": birth, "cwd": "/tmp"}}),
        )
        .await;
        keys.push(reg["key"].as_str().unwrap().to_string());
    }
    let (a, b) = (keys[0].clone(), keys[1].clone());
    let assign = |rid: &str, extra: Value| {
        let mut m = json!({"id": 10, "op": "send", "from": a, "to": b, "kind": "assign",
            "text": "x", "request_id": rid, "done_when": "y"});
        for (k, v) in extra.as_object().unwrap() {
            m[k] = v.clone();
        }
        m
    };

    let sent = call(
        &mut l,
        &mut w,
        json!({"id": 3, "op": "send", "from": a, "to": b, "kind": "assign",
        "text": "run the tests", "done_when": "suite green", "deadline_s": 120,
        "request_id": "as1"}),
    )
    .await;
    assert_eq!(sent["ok"], true);
    assert_eq!(sent["receipt"]["state"], "stored");

    let got = call(&mut l, &mut w, json!({"id": 4, "op": "inbox", "key": b})).await;
    let m = &got["messages"][0];
    assert_eq!(got["messages"].as_array().unwrap().len(), 1);
    assert_eq!(m["kind"], "assign");
    assert_eq!(m["done_when"], "suite green");
    assert!(m["framed"]
        .as_str()
        .unwrap()
        .starts_with("[Lightbulb assignment from "));
    let at = m["deadline_at"].as_i64().unwrap();
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!((at - (t + 120)).abs() <= 5, "deadline_at {at} vs {t}");
    let assign_id = m["id"].as_str().unwrap().to_string();

    let sent = call(
        &mut l,
        &mut w,
        json!({"id": 5, "op": "send", "from": b, "text": "all green",
        "request_id": "re1", "reply_to": "as1"}),
    )
    .await;
    assert_eq!(sent["receipt"]["state"], "stored");
    let got = call(&mut l, &mut w, json!({"id": 6, "op": "inbox", "key": a})).await;
    let m = &got["messages"][0];
    assert_eq!(m["kind"], "result");
    assert_eq!(m["status"], "done");
    assert_eq!(m["reply_to"], assign_id);
    let f = m["framed"].as_str().unwrap();
    assert!(f.starts_with("[Lightbulb result from "), "{f}");
    assert!(f.contains("\"replyTo\":\"as1\""), "{f}");

    let r = call(
        &mut l,
        &mut w,
        json!({"id": 7, "op": "receipt", "from": a, "request_id": "as1"}),
    )
    .await;
    assert_eq!(
        r["receipt"]["result"],
        json!({"request_id": "re1", "status": "done"})
    );

    let r = call(&mut l, &mut w, assign("as6", json!({}))).await;
    assert_eq!(r["receipt"]["state"], "stored");
    let got = call(&mut l, &mut w, json!({"id": 13, "op": "inbox", "key": b})).await;
    let m = got["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["request_id"] == "as6")
        .unwrap();
    let at = m["deadline_at"].as_i64().unwrap();
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(
        (at - (t + 3600)).abs() <= 5,
        "default deadline_at {at} vs {t}"
    );

    for (rid, extra) in [
        ("as7", json!({"deadline_s": 60.0})),
        ("as8", json!({"deadline_s": -5})),
        ("as2", json!({"deadline_s": 20000})),
        ("as4", json!({"deadline_s": 0})),
        ("as5", json!({"deadline_s": "soon"})),
    ] {
        let r = call(&mut l, &mut w, assign(rid, extra)).await;
        assert_eq!(r["ok"], false, "{rid}");
        assert_eq!(r["error"], "bad_request", "{rid}");
    }
    let r = call(
        &mut l,
        &mut w,
        json!({"id": 11, "op": "send", "from": a, "to": b, "kind": "assign",
        "text": "x", "request_id": "as3"}),
    )
    .await;
    assert_eq!(r["receipt"]["state"], "refused");
    assert_eq!(r["receipt"]["reason"], "bad_request");
    call(&mut l, &mut w, json!({"id": 12, "op": "shutdown"})).await;
}

/// Two registered sessions (codex `a`, claude `b`) with `a`'s assignment `as1` (1 s deadline) to
/// `b` sent and left to lapse; returns the connection, the keys and the assignment's id.
async fn lapsed_assignment(
    dir: &tempfile::TempDir,
) -> (
    Lines<BufReader<OwnedReadHalf>>,
    OwnedWriteHalf,
    String,
    String,
    String,
) {
    let claude_home = tempfile::tempdir().unwrap();
    std::env::set_var("LIGHTBULB_CONNECT_CLAUDE_HOME", claude_home.path());
    let home = dir.path().to_path_buf();
    tokio::spawn(async move { daemon::run(&home).await.unwrap() });
    let (mut l, mut w) = connect(&dir.path().join(paths::SOCKET)).await;
    let birth = os::process(std::process::id()).unwrap().birth_micros;
    let mut keys = vec![];
    for (i, (provider, conv)) in [("codex", "a"), ("claude", "b")].into_iter().enumerate() {
        let reg = call(
            &mut l,
            &mut w,
            json!({"id": i + 1, "op": "register", "session": {
            "provider": provider, "conversation": conv, "pid": std::process::id(),
            "birth_micros": birth, "cwd": "/tmp"}}),
        )
        .await;
        keys.push(reg["key"].as_str().unwrap().to_string());
    }
    let (a, b) = (keys[0].clone(), keys[1].clone());
    let sent = call(
        &mut l,
        &mut w,
        json!({"id": 3, "op": "send", "from": a, "to": b, "kind": "assign",
        "text": "x", "done_when": "y", "deadline_s": 1, "request_id": "as1"}),
    )
    .await;
    assert_eq!(sent["receipt"]["state"], "stored");
    let got = call(&mut l, &mut w, json!({"id": 4, "op": "inbox", "key": b})).await;
    let assign_id = got["messages"][0]["id"].as_str().unwrap().to_string();
    tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
    (l, w, a, b, assign_id)
}

#[tokio::test]
async fn an_assignment_past_its_deadline_fails_timed_out_in_the_askers_inbox() {
    let dir = tempfile::tempdir().unwrap();
    let (mut l, mut w, a, b, assign_id) = lapsed_assignment(&dir).await;
    let got = call(
        &mut l,
        &mut w,
        json!({"id": 5, "op": "inbox", "key": a, "wait_secs": 0}),
    )
    .await;
    let ms = got["messages"].as_array().unwrap();
    assert_eq!(ms.len(), 1, "{got}");
    assert_eq!(ms[0]["kind"], "result");
    assert_eq!(ms[0]["status"], "failed");
    assert_eq!(ms[0]["reason"], "timed_out");
    assert_eq!(ms[0]["reply_to"], assign_id);
    assert_eq!(ms[0]["from"], b);
    let r = call(
        &mut l,
        &mut w,
        json!({"id": 6, "op": "receipt", "from": a, "request_id": "as1"}),
    )
    .await;
    assert_eq!(r["receipt"]["result"]["status"], "failed");
}

#[tokio::test]
async fn a_late_result_after_the_timeout_is_mail_not_a_second_closing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut l, mut w, a, b, assign_id) = lapsed_assignment(&dir).await;
    let got = call(
        &mut l,
        &mut w,
        json!({"id": 5, "op": "inbox", "key": a, "wait_secs": 0}),
    )
    .await;
    assert_eq!(got["messages"].as_array().unwrap().len(), 1);
    let sent = call(
        &mut l,
        &mut w,
        json!({"id": 6, "op": "send", "from": b, "text": "done late",
        "request_id": "late1", "reply_to": "as1"}),
    )
    .await;
    assert_eq!(sent["receipt"]["state"], "stored");
    let got = call(&mut l, &mut w, json!({"id": 7, "op": "inbox", "key": a})).await;
    let ms = got["messages"].as_array().unwrap();
    assert_eq!(ms.len(), 1, "{got}");
    assert_eq!(ms[0]["kind"], "result");
    assert_eq!(ms[0]["status"], "done");
    let r = call(
        &mut l,
        &mut w,
        json!({"id": 8, "op": "receipt", "from": a, "request_id": "as1"}),
    )
    .await;
    assert_eq!(
        r["receipt"]["result"]["request_id"],
        format!("timeout-{assign_id}")
    );
}

/// An assignment to this computer's own `<me>/claude:new` (policy: this key allows itself) is
/// stored with no mailbox and handed to `workers::start`, which (no Claude Code driver in this
/// build yet: Task 5) refuses it `no_worker` and starts nothing; its cancel is stored, in no
/// mailbox either. (Task 5's `tests/workers.rs` runs the worker itself.)
#[tokio::test]
async fn an_assign_to_my_own_new_claude_is_stored_and_a_cancel_reaches_the_worker_module() {
    let dir = tempfile::tempdir().unwrap();
    let claude_home = tempfile::tempdir().unwrap();
    std::env::set_var("LIGHTBULB_CONNECT_CLAUDE_HOME", claude_home.path());
    std::fs::write(
        dir.path().join("config.toml"),
        "lan = false\nworkers_offer = \"claude\"\n",
    )
    .unwrap();
    let home = dir.path().to_path_buf();
    tokio::spawn(async move { daemon::run(&home).await.unwrap() });
    let sock = dir.path().join(paths::SOCKET);
    let (mut l, mut w) = connect(&sock).await;
    let birth = os::process(std::process::id()).unwrap().birth_micros;
    let reg = call(
        &mut l,
        &mut w,
        json!({"id": 1, "op": "register", "session": {
        "provider": "codex", "conversation": "a", "pid": std::process::id(),
        "birth_micros": birth, "cwd": "/tmp"}}),
    )
    .await;
    let asker = reg["key"].as_str().unwrap().to_string();
    let me = asker.split('/').next().unwrap();
    let new = format!("{me}/claude:new");

    let sent = call(
        &mut l,
        &mut w,
        json!({"id": 2, "op": "send", "from": asker, "to": new, "kind": "assign",
        "text": "start fresh", "done_when": "green", "deadline_s": 300, "request_id": "as1"}),
    )
    .await;
    assert_eq!(sent["ok"], true, "{sent}");
    assert_eq!(sent["receipt"]["state"], "stored", "{sent}");
    let db = dir.path().join("state.db");
    let store = lightbulb::store::Store::open(&db).unwrap();
    let receipt = json!({"id": 9, "op": "receipt", "from": asker, "request_id": "as1"});
    let r = call(&mut l, &mut w, receipt).await;
    assert_eq!(
        (&r["receipt"]["state"], &r["receipt"]["reason"]),
        (&json!("refused"), &json!("no_worker")),
        "{r}"
    );
    assert!(store.workers(false).unwrap().is_empty());
    // No mailbox holds it.
    let rows = |sql: &str| {
        rusqlite::Connection::open(&db)
            .unwrap()
            .query_row(sql, [], |r| r.get::<_, i64>(0))
            .unwrap()
    };
    assert_eq!(rows("SELECT COUNT(*) FROM mailbox"), 0);
    assert_eq!(rows("SELECT COUNT(*) FROM holds"), 0);

    let cancelled = call(
        &mut l,
        &mut w,
        json!({"id": 3, "op": "send", "from": asker, "reply_to": "as1", "kind": "cancel",
        "text": "stop", "request_id": "c1"}),
    )
    .await;
    assert_eq!(cancelled["receipt"]["state"], "stored", "{cancelled}");
    assert!(store.workers(false).unwrap().is_empty());
    assert_eq!(rows("SELECT COUNT(*) FROM mailbox"), 0);
}
