use lightbulb::{daemon, paths};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

async fn connect(dir: &Path) -> (Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf) {
    for _ in 0..50 {
        if let Ok(s) = UnixStream::connect(dir.join(paths::SOCKET)).await {
            let (r, w) = s.into_split();
            return (BufReader::new(r).lines(), w);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("daemon socket never came up");
}

async fn call(
    lines: &mut Lines<BufReader<OwnedReadHalf>>,
    w: &mut OwnedWriteHalf,
    req: Value,
) -> Value {
    w.write_all(format!("{req}\n").as_bytes()).await.unwrap();
    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap()
}

#[tokio::test]
async fn identity_get_matches_status() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    std::env::set_var("LIGHTBULB_CONNECT_CLAUDE_HOME", dir.path());
    std::fs::write(dir.path().join("config.toml"), "computer = \"mac-b\"\n").unwrap();
    tokio::spawn(async move { daemon::run(&home).await.unwrap() });
    let (mut lines, mut w) = connect(dir.path()).await;

    let status = call(&mut lines, &mut w, json!({"id": 1, "op": "status"})).await;
    let me = call(&mut lines, &mut w, json!({"id": 2, "op": "identity.get"})).await;
    assert_eq!(me["ok"], true);
    assert_eq!(me["member_key_short"], status["member_key_short"]);
    let key = me["member_key"].as_str().unwrap();
    assert_eq!(key.len(), 52);
    assert!(key.starts_with(me["member_key_short"].as_str().unwrap()));
    assert_eq!(me["computer"], "mac-b");
    assert_eq!(me["person_id"], key);
    assert!(me["name"].as_str().is_some_and(|n| !n.is_empty()));

    call(&mut lines, &mut w, json!({"id": 3, "op": "shutdown"})).await;
}
