use lightbulb_network::{daemon, paths};
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
async fn ipc_answers_ping_and_rejects_unknown_op() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    // No Claude sessions to discover: keep `status.sessions` off the real ~/.claude.
    std::env::set_var("LIGHTBULB_NETWORK_CLAUDE_HOME", dir.path());
    let daemon = tokio::spawn(async move { daemon::run(&home).await.unwrap() });
    let (mut lines, mut w) = connect(dir.path()).await;

    assert_eq!(
        call(&mut lines, &mut w, json!({"id": 1, "op": "ping"})).await,
        json!({"id": 1, "ok": true})
    );
    assert_eq!(
        call(&mut lines, &mut w, json!({"id": 2, "op": "nope"})).await,
        json!({"id": 2, "ok": false, "error": "unknown_op"})
    );
    assert_eq!(
        call(&mut lines, &mut w, json!({"id": 3, "op": "send"})).await,
        json!({"id": 3, "ok": false, "error": "unsupported"})
    );
    assert_eq!(
        call(&mut lines, &mut w, json!({"op": "ping"})).await,
        json!({"id": 0, "ok": false, "error": "bad_request"})
    );
    let status = call(&mut lines, &mut w, json!({"id": 4, "op": "status"})).await;
    assert_eq!(status["ok"], true);
    assert_eq!(status["protocol"], 1);
    assert_eq!(status["sessions"], 0);
    assert_eq!(status["member_key_short"].as_str().unwrap().len(), 12);
    assert!(status["team"]["id"]
        .as_str()
        .unwrap()
        .starts_with(status["member_key_short"].as_str().unwrap()));

    // A subscriber sees the shutdown event; the daemon exits cleanly.
    let (mut sub_lines, mut sub_w) = connect(dir.path()).await;
    assert_eq!(
        call(
            &mut sub_lines,
            &mut sub_w,
            json!({"id": 5, "op": "subscribe"})
        )
        .await,
        json!({"id": 5, "ok": true})
    );
    assert_eq!(
        call(&mut lines, &mut w, json!({"id": 6, "op": "shutdown"})).await,
        json!({"id": 6, "ok": true})
    );
    let ev: Value = serde_json::from_str(&sub_lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(ev, json!({"event": "shutdown"}));
    tokio::time::timeout(Duration::from_secs(5), daemon)
        .await
        .unwrap()
        .unwrap();
    assert!(!dir.path().join(paths::SOCKET).exists());
}

#[tokio::test]
async fn two_ensure_running_calls_leave_one_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let exe = Path::new(env!("CARGO_BIN_EXE_lightbulb-network"));
    let calls: Vec<_> = (0..2)
        .map(|_| {
            let d = dir.path().to_path_buf();
            tokio::task::spawn_blocking(move || daemon::ensure_running_at(exe, &d).unwrap())
        })
        .collect();
    for c in calls {
        c.await.unwrap();
    }

    let pid = std::fs::read_to_string(dir.path().join("daemon.lock")).unwrap();
    let pid: u32 = pid.trim().parse().expect("one pid in daemon.lock");
    let alive = |pid: u32| {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    };
    assert!(alive(pid));

    let (mut lines, mut w) = connect(dir.path()).await;
    assert_eq!(
        call(&mut lines, &mut w, json!({"id": 1, "op": "shutdown"})).await,
        json!({"id": 1, "ok": true})
    );
    for _ in 0..50 {
        if !alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("daemon {pid} did not exit after shutdown");
}

#[tokio::test]
async fn ensure_running_right_after_shutdown_starts_a_new_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let exe = Path::new(env!("CARGO_BIN_EXE_lightbulb-network"));
    let d = dir.path().to_path_buf();
    tokio::task::spawn_blocking(move || daemon::ensure_running_at(exe, &d).unwrap())
        .await
        .unwrap();

    let (mut lines, mut w) = connect(dir.path()).await;
    assert_eq!(
        call(&mut lines, &mut w, json!({"id": 1, "op": "shutdown"})).await,
        json!({"id": 1, "ok": true})
    );
    let d = dir.path().to_path_buf();
    tokio::task::spawn_blocking(move || daemon::ensure_running_at(exe, &d).unwrap())
        .await
        .unwrap();

    let (mut lines, mut w) = connect(dir.path()).await;
    assert_eq!(
        call(&mut lines, &mut w, json!({"id": 2, "op": "ping"})).await,
        json!({"id": 2, "ok": true})
    );
    call(&mut lines, &mut w, json!({"id": 3, "op": "shutdown"})).await;
}

/// A daemon still flushing on shutdown holds the lock with no listener: the
/// first spawned child loses the lock and exits, so ensure_running must respawn.
#[tokio::test]
async fn ensure_running_respawns_when_a_dying_daemon_holds_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let exe = Path::new(env!("CARGO_BIN_EXE_lightbulb-network"));
    let lock = std::fs::File::create(dir.path().join("daemon.lock")).unwrap();
    lock.try_lock().unwrap();
    let d = dir.path().to_path_buf();
    let ensure = tokio::task::spawn_blocking(move || daemon::ensure_running_at(exe, &d));
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(lock);
    ensure.await.unwrap().unwrap();

    let (mut lines, mut w) = connect(dir.path()).await;
    call(&mut lines, &mut w, json!({"id": 1, "op": "shutdown"})).await;
}
