//! Push into Claude Code: the daemon posts a user frame into the session's messaging socket.
use lightbulb_network::{daemon, frame, os, paths};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};

async fn connect(sock: &Path) -> (Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf) {
    for _ in 0..50 {
        if let Ok(s) = UnixStream::connect(sock).await {
            let (r, w) = s.into_split();
            return (BufReader::new(r).lines(), w);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
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

/// One Claude home for the whole binary (the env var is process-wide): this process is
/// registered as two Claude sessions, `ok-conv` (socket `ok.sock`) and `gone-conv` (`gone.sock`).
fn claude_home() -> &'static Path {
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    HOME.get_or_init(|| {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude/sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let me = std::process::id();
        let started = os::process(me).unwrap().birth_micros / 1000;
        for (file, conv, sock) in [("a", "ok-conv", "ok.sock"), ("b", "gone-conv", "gone.sock")] {
            let entry = json!({"pid": me, "sessionId": conv, "cwd": "/tmp", "startedAt": started,
                "kind": "interactive", "status": "idle",
                "messagingSocketPath": home.path().join(sock)});
            std::fs::write(dir.join(format!("{file}.json")), entry.to_string()).unwrap();
        }
        std::env::set_var("LIGHTBULB_NETWORK_CLAUDE_HOME", home.path());
        std::env::set_var("PATH", "/usr/bin:/bin"); // no real `codex` for this daemon
        home
    })
    .path()
}

/// Starts a daemon; returns its connection and the key of Claude session `conv`, which it lists as push tier.
async fn start(
    conv: &str,
) -> (
    tempfile::TempDir,
    Lines<BufReader<OwnedReadHalf>>,
    OwnedWriteHalf,
    String,
) {
    claude_home();
    let dir = tempfile::tempdir().unwrap();
    let home: PathBuf = dir.path().into();
    tokio::spawn(async move { daemon::run(&home).await.unwrap() });
    let (mut l, mut w) = connect(&dir.path().join(paths::SOCKET)).await;
    let short = call(&mut l, &mut w, json!({"id": 1, "op": "status"})).await["member_key_short"]
        .as_str()
        .unwrap()
        .to_string();
    let key = format!("{short}/claude:{conv}");
    let s = call(&mut l, &mut w, json!({"id": 2, "op": "sessions"})).await;
    let mine = s["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == key.as_str())
        .cloned();
    assert_eq!(mine.unwrap_or_default()["tier"], "push", "{s}");
    (dir, l, w, key)
}

async fn receipt(
    l: &mut Lines<BufReader<OwnedReadHalf>>,
    w: &mut OwnedWriteHalf,
    rid: &str,
) -> Value {
    call(
        l,
        w,
        json!({"id": 9, "op": "receipt", "from": "x/codex:a", "request_id": rid}),
    )
    .await["receipt"]["state"]
        .clone()
}

#[tokio::test]
async fn claude_push_posts_a_user_frame_to_the_inbox_socket_and_marks_delivered() {
    let listener = UnixListener::bind(claude_home().join("ok.sock")).unwrap();
    let (_dir, mut l, mut w, to) = start("ok-conv").await;

    let sent = call(
        &mut l,
        &mut w,
        json!({"id": 3, "op": "send", "from": "x/codex:a",
        "to": to, "text": "hello claude", "request_id": "r1"}),
    )
    .await;
    assert_eq!(sent["receipt"]["state"], "stored", "{sent}");

    let (conn, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut got = String::new();
    BufReader::new(conn).read_line(&mut got).await.unwrap();
    let framed = frame::peer_from_provider("codex", None, "r1", "hello claude");
    assert_eq!(
        got,
        format!(
            "{{\"from\":\"lightbulb-teammate\",\"message\":{{\"content\":{},\"role\":\"user\"}},\"type\":\"user\"}}\n",
            serde_json::to_string(&framed).unwrap()
        )
    );

    let mut state = Value::Null;
    for _ in 0..50 {
        state = receipt(&mut l, &mut w, "r1").await;
        if state == "delivered" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(state, "delivered");
    let unread = call(&mut l, &mut w, json!({"id": 4, "op": "unread", "key": to})).await;
    assert_eq!(unread["unread"], 0);
}

#[tokio::test]
async fn a_failed_push_leaves_the_mail_for_the_next_hook() {
    // The socket file exists (so the session is push tier) but nothing listens on it.
    drop(UnixListener::bind(claude_home().join("gone.sock")).unwrap());
    let (_dir, mut l, mut w, to) = start("gone-conv").await;

    let sent = call(
        &mut l,
        &mut w,
        json!({"id": 2, "op": "send", "from": "x/codex:a",
        "to": to, "text": "kept", "request_id": "r2"}),
    )
    .await;
    assert_eq!(sent["receipt"]["state"], "stored", "{sent}");
    tokio::time::sleep(Duration::from_millis(500)).await; // the push has been tried and failed

    assert_eq!(receipt(&mut l, &mut w, "r2").await, "stored");
    let unread = call(&mut l, &mut w, json!({"id": 3, "op": "unread", "key": to})).await;
    assert_eq!(unread["unread"], 1);
    let got = call(&mut l, &mut w, json!({"id": 4, "op": "inbox", "key": to})).await;
    assert_eq!(got["messages"][0]["text"], "kept");
    assert_eq!(receipt(&mut l, &mut w, "r2").await, "delivered");
}
