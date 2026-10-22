//! Push into Codex: the daemon runs `codex queue --thread <id> --message <framed>`.
use lightbulb_network::{daemon, frame, os, paths};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

async fn call(
    l: &mut Lines<BufReader<OwnedReadHalf>>,
    w: &mut OwnedWriteHalf,
    req: Value,
) -> Value {
    w.write_all(format!("{req}\n").as_bytes()).await.unwrap();
    serde_json::from_str(&l.next_line().await.unwrap().unwrap()).unwrap()
}

/// A fake `codex` first on PATH records its argv (NUL-separated) and stdin, then answers
/// the way `codex queue` does.
#[tokio::test]
async fn codex_push_runs_codex_queue_with_the_thread_id() {
    let bin = tempfile::tempdir().unwrap();
    let fake = bin.path().join("codex");
    std::fs::write(
        &fake,
        "#!/bin/sh\nd=$(dirname \"$0\")\nprintf '%s\\0' \"$@\" > \"$d/argv\"\ncat > \"$d/stdin\"\n\
         echo \"Queued message 01a08ec5-b961-77c0-962d-25097fe0f5f4 for thread $3.\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("PATH", format!("{}:/usr/bin:/bin", bin.path().display()));
    let claude_home = tempfile::tempdir().unwrap();
    std::env::set_var("LIGHTBULB_NETWORK_CLAUDE_HOME", claude_home.path());

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    tokio::spawn(async move { daemon::run(&home).await.unwrap() });
    let sock = dir.path().join(paths::SOCKET);
    let mut conn = None;
    for _ in 0..50 {
        if let Ok(s) = UnixStream::connect(&sock).await {
            conn = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (r, mut w) = conn.expect("daemon socket never came up").into_split();
    let mut l = BufReader::new(r).lines();

    let birth = os::process(std::process::id()).unwrap().birth_micros;
    let reg = call(
        &mut l,
        &mut w,
        json!({"id": 1, "op": "register", "session": {
        "provider": "codex", "conversation": "t1", "pid": std::process::id(),
        "birth_micros": birth, "cwd": "/tmp"}}),
    )
    .await;
    let to = reg["key"].as_str().unwrap().to_string();
    let s = call(&mut l, &mut w, json!({"id": 2, "op": "sessions"})).await;
    assert_eq!(s["sessions"][0]["tier"], "push", "{s}");

    let sent = call(
        &mut l,
        &mut w,
        json!({"id": 3, "op": "send", "from": "x/claude:a",
        "to": to, "text": "hello codex", "request_id": "r1"}),
    )
    .await;
    assert_eq!(sent["receipt"]["state"], "stored", "{sent}");

    let mut state = Value::Null;
    for _ in 0..50 {
        state = call(
            &mut l,
            &mut w,
            json!({"id": 4, "op": "receipt", "from": "x/claude:a",
            "request_id": "r1"}),
        )
        .await["receipt"]["state"]
            .clone();
        if state == "delivered" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(state, "delivered");

    let framed = frame::peer_from_provider("claude", None, "r1", "hello codex");
    let argv = std::fs::read(bin.path().join("argv")).unwrap();
    let argv: Vec<&[u8]> = argv
        .strip_suffix(b"\0")
        .unwrap()
        .split(|b| *b == 0)
        .collect();
    assert_eq!(
        argv,
        [
            b"queue".as_slice(),
            b"--thread",
            b"t1",
            b"--message",
            framed.as_bytes()
        ]
    );
    assert_eq!(std::fs::read(bin.path().join("stdin")).unwrap(), b"");
    let unread = call(&mut l, &mut w, json!({"id": 5, "op": "unread", "key": to})).await;
    assert_eq!(unread["unread"], 0);
}
