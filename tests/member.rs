//! `lightbulb member`: the engine as a cloud workspace's sidecar, one more member of its
//! team's network, driven by the Lightbulb bridge over a token-gated Unix socket.

mod common;

use common::{identity, line, member, ok, until, Peer};
use iroh::endpoint::VarInt;
use lightbulb::identity::Identity;
use lightbulb::net::frames::Frame;
use lightbulb::net::SUPERSEDED;
use lightbulb::roster::{self, Entry, Kind};
use lightbulb::store::Store;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Child;
use std::time::Duration;

const NAME: &str = "The Lightbulb Company";

fn friday() -> Value {
    json!([{"name": "friday", "agent": "hermes", "title": "Friday", "agentId": "a-1"}])
}

/// A team stored in `dir/home` before the member starts: A ("prasiddha" on "mac", founder)
/// and the member itself, which accepts A's mail (`people`). Returns A.
fn seed(dir: &Path) -> Identity {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let w = Identity::load_or_create(&home).unwrap();
    let a = identity();
    let g = Entry::signed(
        &a,
        vec![],
        Kind::Admit,
        json!({"member": a.key, "name": "prasiddha", "computer": "mac",
            "role": "founder", "ticket_id": "", "team_name": "acme"}),
        1,
    );
    let aw = common::admit(&a, &g, &w, NAME, json!({}));
    let store = Store::open(&home.join("state.db")).unwrap();
    for e in [&g, &aw] {
        roster::accept(&store, e, 10).unwrap();
    }
    a
}

/// `who` dialing the member behind `sock`: the connection, `hello` sent.
async fn dial(sock: &Path, token: &str, who: &Identity) -> (Peer, iroh::Endpoint) {
    dial_as(sock, token, who, None).await
}

/// `dial`, `who` signed in as Lightbulb user `user` (a test-issuer proof in its `hello`).
async fn dial_as(
    sock: &Path,
    token: &str,
    who: &Identity,
    user: Option<&str>,
) -> (Peer, iroh::Endpoint) {
    let key = ok(sock, token, "identity.get", json!({}))["member_key"].clone();
    let port = ok(sock, token, "status", json!({}))["port"]
        .as_u64()
        .unwrap();
    let proof = user.map(|u| common::issuer::proof(&who.key, u, common::now_s()));
    common::dial_at_with(key.as_str().unwrap(), port, who, proof).await
}

/// `p` (member `who`) sends `text` to `to`: its envelope id and the member's ack state and reason.
async fn send_acked(
    p: &mut Peer,
    who: &Identity,
    to: &str,
    text: &str,
) -> (String, String, Option<String>) {
    let id = common::ulid();
    p.send(&Frame::Envelope {
        id: id.clone(),
        request_id: common::ulid(),
        kind: "message".into(),
        from: format!("{}/codex:c1", who.short),
        to: to.into(),
        reply_to: None,
        thread: None,
        hops: 0,
        body: text.into(),
        files: vec![],
        sent_at: 0,
    })
    .await;
    match p
        .recv_where(|f| matches!(f, Frame::Ack { id: i, .. } if *i == id))
        .await
    {
        Frame::Ack { state, reason, .. } => (id, state, reason),
        _ => unreachable!(),
    }
}

/// `send_acked` without the reason.
async fn send(p: &mut Peer, who: &Identity, to: &str, text: &str) -> (String, String) {
    let (id, state, _) = send_acked(p, who, to, text).await;
    (id, state)
}

/// The receipt state `p` hears next for envelope `id`.
async fn receipt(p: &mut Peer, id: &str) -> String {
    match p
        .recv_where(|f| matches!(f, Frame::Receipt { id: i, .. } if i == id))
        .await
    {
        Frame::Receipt { state, .. } => state,
        _ => unreachable!(),
    }
}

fn exits_zero(mut child: Child) {
    let mut code = None;
    until(10, "the member exits", || {
        code = child.try_wait().unwrap();
        code.is_some()
    });
    assert!(code.unwrap().success(), "{code:?}");
}

fn short(sock: &Path, token: &str) -> String {
    ok(sock, token, "identity.get", json!({}))["member_key_short"]
        .as_str()
        .unwrap()
        .into()
}

#[test]
fn a_line_without_the_token_is_refused_and_closed() {
    let dir = tempfile::tempdir().unwrap();
    let (child, token, sock) = member(dir.path(), json!([]));
    for req in [
        json!({"id": 3, "op": "status"}),
        json!({"id": 3, "op": "status", "token": "wrong"}),
        json!({"id": 3, "op": "status", "token": format!("{token}x")}),
    ] {
        let mut s = UnixStream::connect(&sock).unwrap();
        writeln!(s, "{req}").unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut answer = String::new();
        r.read_line(&mut answer).unwrap();
        let answer: Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(
            answer,
            json!({"id": 3, "ok": false, "error": "unauthorized"})
        );
        // Closed: the next line gets nothing back.
        let _ = writeln!(s, "{}", json!({"id": 4, "op": "status", "token": token}));
        let mut more = String::new();
        assert_eq!(r.read_line(&mut more).unwrap_or(0), 0, "{more}");
    }
    // A line longer than 1 MiB is not read to its end: refused and closed.
    let mut s = UnixStream::connect(&sock).unwrap();
    let long = format!(
        "{{\"token\": \"{token}\", \"pad\": \"{}\"}}\n",
        "x".repeat(2 << 20)
    );
    let _ = s.write_all(long.as_bytes()); // the member may close before it is all written
    let mut answer = String::new();
    BufReader::new(s).read_line(&mut answer).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&answer).unwrap()["error"],
        "unauthorized"
    );
    assert_eq!(ok(&sock, &token, "ping", json!({})), json!({}));
    // SAFETY: kill takes a pid and a signal; the pid is our own live child's.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    exits_zero(child);
}

#[test]
fn status_answers_as_an_instance_and_shutdown_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (child, token, sock) = member(dir.path(), json!([]));
    let s = ok(&sock, &token, "status", json!({}));
    assert_eq!(s["version"], env!("CARGO_PKG_VERSION"), "{s}");
    assert_eq!(s["team"]["name"], NAME, "{s}");
    let r = line(&sock, &token, "shutdown", json!({}));
    assert_eq!(
        (r["ok"].clone(), r["error"].clone(), r["id"].clone()),
        (json!(false), json!("unsupported"), json!(7)),
        "{r}"
    );
    // A person-only op answers as for an instance's caller.
    let r = line(&sock, &token, "team.invite", json!({}));
    assert_eq!(r["error"], "unsupported", "{r}");
    // Still running: SIGTERM stops it, exit 0, with its snapshot.
    assert_eq!(ok(&sock, &token, "ping", json!({})), json!({}));
    // SAFETY: kill takes a pid and a signal; the pid is our own live child's.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    exits_zero(child);
    assert!(dir.path().join("last.db").is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_early_message_to_a_declared_session_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let a = seed(dir.path());
    // A bad name and a second friday are skipped: the member starts with friday declared.
    let mut sessions = friday();
    let list = sessions.as_array_mut().unwrap();
    list.push(json!({"name": "Not a name!", "agent": "hermes"}));
    list.push(json!({"name": "friday", "agent": "other"}));
    let (child, token, sock) = member(dir.path(), sessions);
    // No `cloud.sessions.set`: the sessions file declared friday before the member started.
    let (mut pa, _ep) = dial(&sock, &token, &a).await;
    let to = format!("{}/friday", short(&sock, &token));
    assert_eq!(send(&mut pa, &a, &to, "early").await.1, "stored");
    let ghost = format!("{}/ghost", short(&sock, &token));
    assert_eq!(send(&mut pa, &a, &ghost, "nobody").await.1, "refused");
    let peek = ok(&sock, &token, "inbox.peek", json!({}));
    assert_eq!(peek["sessions"][0]["session"], "friday", "{peek}");
    // Friday's agent was removed: its mail stays with the engine, off the peek.
    ok(&sock, &token, "cloud.sessions.set", json!({"sessions": []}));
    let peek = ok(&sock, &token, "inbox.peek", json!({}));
    assert_eq!(peek["sessions"], json!([]), "{peek}");
    ok(&sock, &token, "stop", json!({}));
    exits_zero(child);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peek_lists_without_taking_and_ack_moves_the_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let a = seed(dir.path());
    let (child, token, sock) = member(dir.path(), friday());
    let (mut pa, _ep) = dial(&sock, &token, &a).await;
    let to = format!("{}/friday", short(&sock, &token));
    let (id, state) = send(&mut pa, &a, &to, "hello friday").await;
    assert_eq!(state, "stored");

    let peek = ok(&sock, &token, "inbox.peek", json!({}));
    assert_eq!(
        peek,
        ok(&sock, &token, "inbox.peek", json!({})),
        "peek takes nothing"
    );
    let groups = peek["sessions"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "{peek}");
    assert_eq!(groups[0]["session"], "friday");
    let records = groups[0]["records"].as_array().unwrap();
    assert_eq!(records.len(), 1, "{peek}");
    let rec = &records[0];
    assert_eq!(
        (rec["id"].as_str(), rec["text"].as_str()),
        (Some(id.as_str()), Some("hello friday"))
    );
    assert!(
        rec["framed"].as_str().unwrap().contains("hello friday"),
        "{rec}"
    );
    let mut keys: Vec<&String> = rec.as_object().unwrap().keys().collect();
    keys.sort();
    let mut want = lightbulb::daemon::ipc::INBOX_RECORD.to_vec();
    want.push("stage");
    want.sort();
    assert_eq!(keys, want);
    assert_eq!(rec["stage"], "waiting");

    let ack = |record: &str, stage: &str| {
        line(
            &sock,
            &token,
            "inbox.ack",
            json!({"session": "friday", "record": record, "stage": stage}),
        )
    };
    assert_eq!(ack(&id, "seen")["error"], "bad_request");
    assert_eq!(ack("nope", "delivered")["error"], "not_found");
    assert_eq!(ack("nope", "read")["error"], "not_found");

    assert_eq!(
        ack(&id, "delivered"),
        json!({"id": 7, "ok": true, "result": {}})
    );
    assert_eq!(receipt(&mut pa, &id).await, "delivered");
    // Delivered, not read (a turn that died): still listed, and a second ack is the same.
    let after = ok(&sock, &token, "inbox.peek", json!({}));
    let listed = &after["sessions"][0]["records"];
    assert_eq!(listed.as_array().map(Vec::len), Some(1), "{after}");
    assert_eq!(
        (listed[0]["id"].as_str(), listed[0]["stage"].as_str()),
        (Some(id.as_str()), Some("delivered"))
    );
    assert_eq!(ack(&id, "delivered")["result"], json!({}));

    assert_eq!(ack(&id, "read")["ok"], true);
    assert_eq!(receipt(&mut pa, &id).await, "read");
    let after = ok(&sock, &token, "inbox.peek", json!({}));
    assert_eq!(after["sessions"], json!([]), "{after}");
    // `read` on a record still waiting takes it too.
    let (id2, _) = send(&mut pa, &a, &to, "second").await;
    assert_eq!(ack(&id2, "read")["ok"], true);
    // Its receipt reaches `read` (a `delivered` may come first or be folded into it).
    pa.recv_where(
        |f| matches!(f, Frame::Receipt { id, state, .. } if *id == id2 && state == "read"),
    )
    .await;
    let after = ok(&sock, &token, "inbox.peek", json!({}));
    assert_eq!(after["sessions"], json!([]), "{after}");
    let db = dir.path().join("home/state.db");
    let sql = format!("SELECT COUNT(*) FROM messages WHERE id = '{id}' AND body IS NULL");
    until(10, "the text is forgotten", || {
        common::count(&db, &sql) == 1
    });
    ok(&sock, &token, "stop", json!({}));
    exits_zero(child);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_writes_the_snapshot_outside_the_home() {
    let dir = tempfile::tempdir().unwrap();
    let (child, token, sock) = member(dir.path(), friday());
    assert_eq!(ok(&sock, &token, "stop", json!({})), json!({}));
    exits_zero(child);
    let snap = Store::open(&dir.path().join("last.db")).unwrap();
    assert!(!roster::entries(&snap).unwrap().is_empty());
    assert!(
        !dir.path().join("sock").exists(),
        "the socket file is removed at stop"
    );
}

// The test plays A's computer closing our link as SUPERSEDED, as it does when a second
// connection from this member's key replaces it (accepted by ruling over dialing twice).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_connection_with_this_key_emits_superseded() {
    let dir = tempfile::tempdir().unwrap();
    let a = seed(dir.path());
    let (child, token, sock) = member(dir.path(), friday());
    let mut sub = UnixStream::connect(&sock).unwrap();
    writeln!(
        sub,
        "{}",
        json!({"token": token, "id": 1, "op": "subscribe"})
    )
    .unwrap();
    sub.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut events = BufReader::new(sub);
    let mut first = String::new();
    events.read_line(&mut first).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&first).unwrap()["ok"],
        true,
        "{first}"
    );
    // A's computer took a second connection from this member's key (another process holding
    // it): it closes ours as superseded.
    let (pa, _ep) = dial(&sock, &token, &a).await;
    until(10, "A is online", || {
        ok(&sock, &token, "status", json!({}))["peers"]["online"] == 1
    });
    pa.conn.close(VarInt::from_u32(SUPERSEDED), b"superseded");
    let short = a.short.clone();
    let seen = tokio::task::spawn_blocking(move || loop {
        let mut l = String::new();
        if events.read_line(&mut l).unwrap() == 0 {
            return None;
        }
        let e: Value = serde_json::from_str(&l).unwrap();
        if e["event"] == "superseded" {
            return Some(e);
        }
    })
    .await
    .unwrap()
    .expect("a superseded event");
    assert_eq!(seen["by_peer"], short.as_str(), "{seen}");
    ok(&sock, &token, "stop", json!({}));
    exits_zero(child);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivered_record_left_unread_past_its_ttl_expires() {
    let dir = tempfile::tempdir().unwrap();
    let a = seed(dir.path());
    let (child, token, sock) = member(dir.path(), friday());
    let (mut pa, _ep) = dial(&sock, &token, &a).await;
    let to = format!("{}/friday", short(&sock, &token));
    let (id, _) = send(&mut pa, &a, &to, "a turn that always fails").await;
    let ack = json!({"session": "friday", "record": id, "stage": "delivered"});
    ok(&sock, &token, "inbox.ack", ack);
    ok(&sock, &token, "stop", json!({}));
    exits_zero(child);
    // Delivered more than `outbox::TTL_S` ago, never read.
    let db = dir.path().join("home/state.db");
    let c = rusqlite::Connection::open(&db).unwrap();
    let old = format!("updated_at - {}", lightbulb::outbox::TTL_S + 10);
    c.execute(
        &format!("UPDATE receipts SET updated_at = {old} WHERE state = 'delivered'"),
        [],
    )
    .unwrap();
    drop(c);
    let (child, token, sock) = member(dir.path(), friday());
    let peek = ok(&sock, &token, "inbox.peek", json!({}));
    assert_eq!(peek["sessions"], json!([]), "{peek}");
    let sql = format!(
        "SELECT COUNT(*) FROM receipts r JOIN messages m ON m.from_key = r.from_key \
         AND m.request_id = r.request_id WHERE m.id = '{id}' AND r.state = 'expired' \
         AND m.body IS NULL"
    );
    until(10, "the receipt reads expired", || {
        common::count(&db, &sql) == 1
    });
    ok(&sock, &token, "stop", json!({}));
    exits_zero(child);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_workspace_member_never_invites() {
    let dir = tempfile::tempdir().unwrap();
    let (child, token, sock) = member(dir.path(), friday());
    let invite = |mode: Value| line(&sock, &token, "team.invite", json!({ "mode": mode }));
    // `ask` and `person` are no `team.invite` modes (person links are gone).
    for mode in [json!("ask"), json!("person")] {
        assert_eq!(invite(mode.clone())["error"], "bad_request", "{mode}");
    }
    for mode in [
        Value::Null,
        json!("open"),
        json!("reusable"),
        json!("once"),
        json!("new"),
    ] {
        assert_eq!(invite(mode.clone())["error"], "unsupported", "{mode}");
    }
    ok(&sock, &token, "stop", json!({}));
    exits_zero(child);
    // A computer refuses a person link too.
    let a = common::Env::new();
    a.daemon(false, "", &[]);
    let r = common::person(
        &a,
        json!({"id": 1, "op": "team.invite", "mode": "person", "for": "u", "name": "P"}),
    );
    assert_eq!(r["error"], "bad_request", "{r}");
}

/// The doorbell (E4): the member founds its own network and publishes `wake` from its
/// sessions file; computer A ("mac"), admitted to it, dials it with a fake `curl` first on its
/// PATH that keeps its argv and stdin under `dir/curl`. Returns A (running), the member, its
/// token and socket, and the wake token.
fn doorbell(dir: &Path) -> (common::Env, Child, String, std::path::PathBuf, String) {
    doorbell_of(dir, true)
}

/// `doorbell`, the member its network's founder or, not, a member of A's network: either way
/// its own `wake` is the one A rings.
fn doorbell_of(
    dir: &Path,
    founder: bool,
) -> (common::Env, Child, String, std::path::PathBuf, String) {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let w = Identity::load_or_create(&home).unwrap();
    let a = common::Env::new();
    let entries: Vec<Entry> = if founder {
        let g = lightbulb::daemon::net_ops::genesis(&w, NAME, NAME, NAME, 1);
        let aa = common::admit(
            &w,
            &g,
            &a.identity(),
            "prasiddha",
            json!({"computer": "mac"}),
        );
        vec![g, aa]
    } else {
        let g = lightbulb::daemon::net_ops::genesis(&a.identity(), "prasiddha", "mac", "acme", 1);
        // Admitted through a once link, as `team.join` does.
        let ai = a.identity();
        let ticket = format!("{}.t1", ai.short);
        let link = Entry::signed(
            &ai,
            vec![g.hash()],
            Kind::Link,
            json!({"ticket_id": ticket, "secret": "s", "mode": "ask", "once": true,
                "expires_at": 99}),
            2,
        );
        let aw = common::admit(
            &ai,
            &link,
            &w,
            NAME,
            json!({"computer": NAME, "ticket_id": ticket}),
        );
        vec![g, link, aw]
    };
    for s in [Store::open(&home.join("state.db")).unwrap(), a.store()] {
        for e in &entries {
            roster::accept(&s, e, 10).unwrap();
        }
    }
    let wake_token = format!("org_1.{}", common::ulid());
    let wake = json!({"url": "https://wake.example.test/api/v1/network/wake", "token": wake_token});
    let file = json!({"workspace": NAME, "sessions": friday(), "wake": wake});
    let (child, token, sock) = common::member_with(dir, file);
    let bin = dir.join("bin");
    std::fs::create_dir_all(dir.join("curl")).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let rec = dir.join("curl").display().to_string();
    let fake = common::script(
        &bin,
        &format!(
            "n=$(ls '{rec}' | wc -l | tr -d ' ')\n\
             printf '%s\\n' \"$@\" > '{rec}/'$n.args\n\
             cat > '{rec}/'$n.stdin"
        ),
    );
    std::fs::rename(fake, bin.join("curl")).unwrap();
    let key = ok(&sock, &token, "identity.get", json!({}))["member_key"].clone();
    let port = ok(&sock, &token, "status", json!({}))["port"].clone();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let peers = format!("{}@127.0.0.1:{port}", key.as_str().unwrap());
    a.daemon(
        false,
        "",
        &[("LIGHTBULB_CONNECT_PEERS", peers), ("PATH", path)],
    );
    common::until(10, "A is online", || {
        ok(&sock, &token, "status", json!({}))["peers"]["online"] == 1
    });
    (a, child, token, sock, wake_token)
}

/// The curl calls made so far: each one's argv lines.
fn rings(dir: &Path) -> Vec<Vec<String>> {
    let mut calls: Vec<_> = std::fs::read_dir(dir.join("curl"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "args"))
        .collect();
    calls.sort();
    let lines = |p: &Path| {
        std::fs::read_to_string(p)
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    };
    calls.iter().map(|p| lines(p)).collect()
}

/// The member stopped, and a message for its Friday due on A: until A rang once. Returns
/// Friday's address.
fn rang(dir: &Path, a: &common::Env, child: Child, token: &str, sock: &Path) -> String {
    let to = format!("{}/friday", short(sock, token));
    let from = common::session(a, "c1");
    // Up: it reaches the member over the connection (stored there), no doorbell.
    a.send(&from, &to, "while up", "r1", None);
    common::until(10, "on the member", || a.state(&from, "r1") == "stored");
    ok(sock, token, "stop", json!({}));
    exits_zero(child);
    assert!(rings(dir).is_empty(), "rang while the member was up");
    a.send(&from, &to, "while parked", "r2", None);
    common::until(90, "A rings the doorbell", || !rings(dir).is_empty());
    to
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_computer_rings_the_doorbell_only_after_a_failed_dial() {
    let dir = tempfile::tempdir().unwrap();
    let (a, child, token, sock, _) = doorbell(dir.path());
    let to = rang(dir.path(), &a, child, &token, &sock);
    // A parked member says so: online false, wakes true, and `who --all` in those words.
    let row = a.ask(json!({"id": 1, "op": "directory"}))["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == to.as_str())
        .cloned()
        .unwrap();
    assert_eq!(
        (&row["online"], &row["wakes"]),
        (&json!(false), &json!(true)),
        "{row}"
    );
    let o = a.detached(&[common::EXE, "who", "--all"], "");
    assert!(
        common::out(&o).contains("parked, wakes when messaged"),
        "{}",
        common::out(&o)
    );
    let calls = rings(dir.path());
    assert_eq!(
        calls[0],
        [
            "-q",
            "-g",
            "--proto",
            "=http,https",
            "-fsS",
            "-m",
            "10",
            "-X",
            "POST",
            "-K",
            "-",
            "https://wake.example.test/api/v1/network/wake"
        ]
    );
    // Backoff: more messages due ring no second time soon.
    a.send(&common::session(&a, "c1"), &to, "still parked", "r3", None);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(rings(dir.path()).len(), 1);
}

/// A member that is not its network's founder publishes its own doorbell, and a
/// computer rings it after a failed dial (the founder case is the test above).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_computer_rings_a_non_founder_members_doorbell() {
    let dir = tempfile::tempdir().unwrap();
    let (a, child, token, sock, wake) = doorbell_of(dir.path(), false);
    // Parked: a message for its Friday is due on A, and A rings the member's own doorbell.
    let to = format!("{}/friday", short(&sock, &token));
    ok(&sock, &token, "stop", json!({}));
    exits_zero(child);
    a.send(&common::session(&a, "c1"), &to, "while parked", "r1", None);
    let stdin = dir.path().join("curl/0.stdin");
    common::until(90, "A rings the doorbell", || {
        std::fs::read_to_string(&stdin).is_ok_and(|t| !t.is_empty())
    });
    assert_eq!(
        std::fs::read_to_string(&stdin).unwrap(),
        format!("header = \"Authorization: Bearer {wake}\"\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_doorbell_token_is_absent_from_logs() {
    let dir = tempfile::tempdir().unwrap();
    let (a, child, token, sock, wake) = doorbell(dir.path());
    rang(dir.path(), &a, child, &token, &sock);
    // On curl's stdin, as a config line: never on its command line. The fake writes its argv
    // before its stdin, so wait for the stdin file itself.
    let path = dir.path().join("curl/0.stdin");
    common::until(30, "curl's stdin is written", || {
        std::fs::read_to_string(&path).is_ok_and(|t| !t.is_empty())
    });
    let stdin = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        stdin,
        format!("header = \"Authorization: Bearer {wake}\"\n")
    );
    assert!(rings(dir.path())[0].iter().all(|l| !l.contains(&wake)));
    for log in [
        dir.path().join("member.log"),
        a.state.path().join("daemon.log"),
    ] {
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(!text.contains(&wake), "{log:?}: {text}");
        assert!(!text.contains("org_1."), "{log:?}");
    }
    assert!(
        std::fs::read_to_string(a.state.path().join("daemon.log"))
            .unwrap()
            .contains("doorbell"),
        "the ring is logged"
    );
}

/// The sessions file's `members` (people) gates who may message the member's sessions: a
/// computer whose sign-in proof names one of them is delivered; any other (another person's
/// proof, or none) is `refused` `not_a_member` (not held, nothing in the mailbox); `null` gates
/// no one; `cloud.sessions.set` replaces the list.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_members_list_gates_who_may_message_the_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let a = seed(dir.path());
    let file = json!({"workspace": NAME, "sessions": friday(), "members": ["user_a", "user_x"]});
    let (child, token, sock) = common::member_with(dir.path(), file);
    let (mut pa, _ep) = dial_as(&sock, &token, &a, Some("user_a")).await;
    let to = format!("{}/friday", short(&sock, &token));
    assert_eq!(send(&mut pa, &a, &to, "in").await.1, "stored");
    // Reloaded without A's person: refused, and nothing more reaches the mailbox.
    ok(
        &sock,
        &token,
        "cloud.sessions.set",
        json!({"sessions": friday(), "members": ["user_x"]}),
    );
    let (_, state, reason) = send_acked(&mut pa, &a, &to, "out").await;
    assert_eq!(
        (state.as_str(), reason.as_deref()),
        ("refused", Some("not_a_member"))
    );
    let peek = ok(&sock, &token, "inbox.peek", json!({}));
    assert_eq!(
        peek["sessions"][0]["records"].as_array().unwrap().len(),
        1,
        "{peek}"
    );
    // Reloaded with `members: null`: no gate.
    ok(
        &sock,
        &token,
        "cloud.sessions.set",
        json!({"sessions": friday(), "members": null}),
    );
    assert_eq!(send(&mut pa, &a, &to, "again").await.1, "stored");
    ok(&sock, &token, "stop", json!({}));
    exits_zero(child);
}

/// No proof, another person's, or one for another computer: refused, however the hello reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_computer_without_a_members_proof_is_refused() {
    for shown in [None, Some("user_b"), Some("other-computer")] {
        let dir = tempfile::tempdir().unwrap();
        let a = seed(dir.path());
        let file = json!({"workspace": NAME, "sessions": friday(), "members": ["user_a"]});
        let (child, token, sock) = common::member_with(dir.path(), file);
        let (mut pa, _ep) = match shown {
            Some("other-computer") => {
                let key = ok(&sock, &token, "identity.get", json!({}))["member_key"].clone();
                let port = ok(&sock, &token, "status", json!({}))["port"]
                    .as_u64()
                    .unwrap();
                let proof =
                    common::issuer::proof(&common::identity().key, "user_a", common::now_s());
                common::dial_at_with(key.as_str().unwrap(), port, &a, Some(proof)).await
            }
            user => dial_as(&sock, &token, &a, user).await,
        };
        let to = format!("{}/friday", short(&sock, &token));
        let (_, state, reason) = send_acked(&mut pa, &a, &to, "no").await;
        assert_eq!(
            (state.as_str(), reason.as_deref()),
            ("refused", Some("not_a_member")),
            "{shown:?}"
        );
        ok(&sock, &token, "stop", json!({}));
        exits_zero(child);
    }
}

/// `members` on `cloud.sessions.set`: absent keeps the gate, `null` removes it, a list replaces
/// it, and anything else (or a list with a non-string) is `bad_request` and changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_set_members_absent_keeps_null_removes_list_replaces_bad_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let a = seed(dir.path());
    let file = json!({"workspace": NAME, "sessions": friday(), "members": ["user_x"]});
    let (child, token, sock) = common::member_with(dir.path(), file);
    let (mut pa, _ep) = dial_as(&sock, &token, &a, Some("user_a")).await;
    let to = format!("{}/friday", short(&sock, &token));
    let state = async |pa: &mut Peer| send_acked(pa, &a, &to, "x").await.1;
    assert_eq!(state(&mut pa).await, "refused");
    // Absent: the gate stays.
    ok(
        &sock,
        &token,
        "cloud.sessions.set",
        json!({"sessions": friday()}),
    );
    assert_eq!(state(&mut pa).await, "refused");
    // Bad values: refused, the gate stays.
    for bad in [json!("p"), json!({"a": 1}), json!([1]), json!(["ok", null])] {
        let r = line(
            &sock,
            &token,
            "cloud.sessions.set",
            json!({"sessions": friday(), "members": bad}),
        );
        assert_eq!(r["error"], "bad_request", "{r}");
    }
    assert_eq!(state(&mut pa).await, "refused");
    // A list replaces it.
    ok(
        &sock,
        &token,
        "cloud.sessions.set",
        json!({"sessions": friday(), "members": ["user_a"]}),
    );
    assert_eq!(state(&mut pa).await, "stored");
    // Null removes it.
    ok(
        &sock,
        &token,
        "cloud.sessions.set",
        json!({"sessions": friday(), "members": null}),
    );
    assert_eq!(state(&mut pa).await, "stored");
    ok(&sock, &token, "stop", json!({}));
    exits_zero(child);
}

/// A sessions file whose `members` is not a list of strings keeps the member from starting.
#[test]
fn a_bad_members_in_the_sessions_file_is_a_startup_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    let file = dir.path().join("sessions.json");
    std::fs::write(
        &file,
        json!({"sessions": [], "members": "everyone"}).to_string(),
    )
    .unwrap();
    let mut child = std::process::Command::new(common::EXE)
        .arg("member")
        .arg("--home")
        .arg(dir.path().join("home"))
        .arg("--snapshot")
        .arg(dir.path().join("last.db"))
        .arg("--socket")
        .arg(dir.path().join("sock"))
        .args(["--name", NAME, "--sessions"])
        .arg(&file)
        .env("HOME", dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "t").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("members"));
}

/// A key in `revoked_keys` is refused even with a valid proof of an admitted person; an absent
/// field is an empty list.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revoked_key_is_refused_even_with_a_valid_proof() {
    let dir = tempfile::tempdir().unwrap();
    let a = seed(dir.path());
    let file = json!({"workspace": NAME, "sessions": friday(), "members": ["user_a"],
        "revoked_keys": [a.key]});
    let (child, token, sock) = common::member_with(dir.path(), file);
    let (mut pa, _ep) = dial_as(&sock, &token, &a, Some("user_a")).await;
    let to = format!("{}/friday", short(&sock, &token));
    let (_, state, reason) = send_acked(&mut pa, &a, &to, "no").await;
    assert_eq!(
        (state.as_str(), reason.as_deref()),
        ("refused", Some("not_a_member"))
    );
    // Reloaded without the field: nobody is revoked.
    ok(
        &sock,
        &token,
        "cloud.sessions.set",
        json!({"sessions": friday(), "members": ["user_a"]}),
    );
    assert_eq!(send(&mut pa, &a, &to, "yes").await.1, "stored");
    // A bad list changes nothing.
    let r = common::line(
        &sock,
        &token,
        "cloud.sessions.set",
        json!({"sessions": friday(), "revoked_keys": "all"}),
    );
    assert_eq!(r["error"], "bad_request", "{r}");
    ok(&sock, &token, "stop", json!({}));
    exits_zero(child);
}

/// `who` signed in as `user`, dialing the member behind `sock` with a `hello` that syncs
/// memory and empty heads: every memory entry the member sends it within 2 s.
async fn memory_from(sock: &Path, token: &str, who: &Identity, user: &str) -> Vec<String> {
    use lightbulb::net::frames;
    let key = ok(sock, token, "identity.get", json!({}))["member_key"].clone();
    let port = ok(sock, token, "status", json!({}))["port"]
        .as_u64()
        .unwrap();
    let ep = common::endpoint(who).await;
    let conn = ep
        .connect(
            common::loopback(key.as_str().unwrap(), port),
            lightbulb::net::ALPN,
        )
        .await
        .unwrap();
    let (send, recv) = conn.open_bi().await.unwrap();
    let mut p = Peer { conn, send, recv };
    let proof = common::issuer::proof(&who.key, user, common::now_s());
    p.hello_full(vec![], Some(proof), true).await;
    p.send(&Frame::MemoryHeads {
        heads: Default::default(),
    })
    .await;
    let mut got = vec![];
    let end = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Ok(f)) = tokio::time::timeout_at(end, frames::read(&mut p.recv)).await {
        if let Frame::Memory { entries, .. } = f {
            got.extend(entries.into_iter().map(|e| e.id));
        }
    }
    got
}

/// A workspace member's own entries (the facts it imports from its rooms) go only to the
/// computers its workspace admits, by the gate its mail uses; a laptop's team entry it holds
/// still goes to every member.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_entries_reach_only_workspace_members() {
    use lightbulb::memory::{self, Kind as Fact, Op, Scope, Source};
    let dir = tempfile::tempdir().unwrap();
    let a = seed(dir.path());
    let home = dir.path().join("home");
    let w = Identity::load_or_create(&home).unwrap();
    let b = identity();
    let store = Store::open(&home.join("state.db")).unwrap();
    let head = roster::entries(&store).unwrap().pop().unwrap();
    roster::accept(&store, &common::admit(&a, &head, &b, "b", json!({})), 10).unwrap();
    let team = |fact: &str| Op::Remember {
        subject: "s".into(),
        fact: fact.into(),
        kind: Fact::Fact,
        scope: Scope::Team,
        when: None,
        source: Source {
            harness: "test".into(),
            repo: None,
            session: None,
            on_behalf_of: None,
            shared_from: None,
        },
        replaces: vec![],
    };
    let room = memory::remember(&store, &w, team("from a workspace room"), 5).unwrap();
    let laptop = memory::remember(&store, &a, team("from a's laptop"), 5).unwrap();
    drop(store);
    let file = json!({"workspace": NAME, "sessions": friday(), "members": ["user_a"]});
    let (child, token, sock) = common::member_with(dir.path(), file);

    let to_a = memory_from(&sock, &token, &a, "user_a").await;
    assert!(
        to_a.contains(&room.id) && to_a.contains(&laptop.id),
        "{to_a:?}"
    );
    let to_b = memory_from(&sock, &token, &b, "user_b").await;
    assert!(to_b.contains(&laptop.id), "{to_b:?}");
    assert!(!to_b.contains(&room.id), "{to_b:?}");

    ok(&sock, &token, "stop", json!({}));
    exits_zero(child);
}
