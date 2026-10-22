use lightbulb_network::{daemon, os, paths};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

async fn call(
    lines: &mut Lines<BufReader<OwnedReadHalf>>,
    w: &mut OwnedWriteHalf,
    req: Value,
) -> Value {
    w.write_all(format!("{req}\n").as_bytes()).await.unwrap();
    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap()
}

/// One test per file: it sets a process-wide env var so the daemon never reads the real ~/.claude.
#[tokio::test]
async fn register_sessions_end_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let claude_home = tempfile::tempdir().unwrap();
    std::env::set_var("LIGHTBULB_NETWORK_CLAUDE_HOME", claude_home.path());
    let home = dir.path().to_path_buf();
    tokio::spawn(async move { daemon::run(&home).await.unwrap() });

    let sock = dir.path().join(paths::SOCKET);
    let mut stream = None;
    for _ in 0..50 {
        if let Ok(s) = UnixStream::connect(&sock).await {
            stream = Some(s);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let (r, mut w) = stream.expect("daemon socket never came up").into_split();
    let mut lines = BufReader::new(r).lines();

    let birth = os::process(std::process::id()).unwrap().birth_micros;
    let reg = call(
        &mut lines,
        &mut w,
        json!({"id": 1, "op": "register", "session": {
        "provider": "codex", "conversation": "c1", "pid": std::process::id(),
        "birth_micros": birth, "cwd": "/tmp/p", "title": "hello"}}),
    )
    .await;
    assert_eq!(reg["ok"], true);
    let key = reg["key"].as_str().unwrap().to_string();
    assert!(key.ends_with("/codex:c1"));

    // Registered but dead (wrong birth): stored, never listed.
    call(
        &mut lines,
        &mut w,
        json!({"id": 2, "op": "register", "session": {
        "provider": "codex", "conversation": "c2", "pid": std::process::id(),
        "birth_micros": 1, "cwd": "/tmp/p"}}),
    )
    .await;

    let list = call(&mut lines, &mut w, json!({"id": 3, "op": "sessions"})).await;
    let sessions = list["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["key"], key);
    assert_eq!(sessions[0]["status"], "idle");
    assert_eq!(sessions[0]["tier"], "pull");
    assert_eq!(sessions[0]["title"], "hello");
    assert_eq!(
        call(&mut lines, &mut w, json!({"id": 4, "op": "status"})).await["sessions"],
        1
    );

    assert_eq!(
        call(
            &mut lines,
            &mut w,
            json!({"id": 5, "op": "end", "key": key})
        )
        .await["ok"],
        true
    );
    assert_eq!(
        call(
            &mut lines,
            &mut w,
            json!({"id": 6, "op": "end", "key": key})
        )
        .await["error"],
        "not_found"
    );
    assert_eq!(
        call(&mut lines, &mut w, json!({"id": 7, "op": "sessions"})).await["sessions"],
        json!([])
    );
    assert_eq!(
        call(
            &mut lines,
            &mut w,
            json!({"id": 8, "op": "register", "session": {}})
        )
        .await["error"],
        "bad_request"
    );
    call(&mut lines, &mut w, json!({"id": 9, "op": "shutdown"})).await;
}
