use lightbulb_network::{daemon, os, paths};
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
    std::env::set_var("LIGHTBULB_NETWORK_CLAUDE_HOME", claude_home.path());
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
    std::env::set_var("LIGHTBULB_NETWORK_CLAUDE_HOME", claude_home.path());
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
    std::env::set_var("LIGHTBULB_NETWORK_CLAUDE_HOME", claude_home.path());
    let home = dir.path().to_path_buf();
    tokio::spawn(async move { daemon::run(&home).await.unwrap() });
    let sock = dir.path().join(paths::SOCKET);
    let (mut l, mut w) = connect(&sock).await;
    let key = "x/codex:gone-reader";

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
