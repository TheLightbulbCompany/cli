use lightbulb_connect::{daemon, paths};
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
    std::env::set_var("LIGHTBULB_CONNECT_CLAUDE_HOME", dir.path());
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
        call(&mut lines, &mut w, json!({"id": 3, "op": "tool"})).await,
        json!({"id": 3, "ok": false, "error": "unknown_op"})
    );
    assert_eq!(
        call(&mut lines, &mut w, json!({"id": 3, "op": "hook"})).await,
        json!({"id": 3, "ok": false, "error": "unknown_op"})
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
    // The team id is the solo network's genesis hash.
    let store = lightbulb_connect::store::Store::open(&dir.path().join("state.db")).unwrap();
    let entries = lightbulb_connect::roster::entries(&store).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(status["team"]["id"], entries[0].hash());

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
    let exe = Path::new(env!("CARGO_BIN_EXE_lightbulb-connect"));
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
    let exe = Path::new(env!("CARGO_BIN_EXE_lightbulb-connect"));
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
    let exe = Path::new(env!("CARGO_BIN_EXE_lightbulb-connect"));
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

/// Two daemons started at once: one serves, the other waits on the lock (alive, not
/// serving) and takes over when the first shuts down.
#[tokio::test]
async fn a_lock_loser_waits_and_takes_over() {
    let dir = tempfile::tempdir().unwrap();
    let start = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_lightbulb-connect"))
            .arg("daemon")
            .env(paths::HOME_ENV, dir.path())
            .env("LIGHTBULB_CONNECT_CLAUDE_HOME", dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    };
    let mut daemons = [start(), start()];
    let holder = |dir: &Path| -> u32 {
        std::fs::read_to_string(dir.join("daemon.lock"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    };
    let (mut lines, mut w) = connect(dir.path()).await;
    assert_eq!(
        call(&mut lines, &mut w, json!({"id": 1, "op": "ping"})).await["ok"],
        true
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    for d in &mut daemons {
        assert!(d.try_wait().unwrap().is_none(), "both daemons stay alive");
    }
    let first = holder(dir.path());
    let (winner, loser) = if daemons[0].id() == first {
        (0, 1)
    } else {
        (1, 0)
    };
    assert_eq!(daemons[winner].id(), first);

    call(&mut lines, &mut w, json!({"id": 2, "op": "shutdown"})).await;
    daemons[winner].wait().unwrap();
    // The waiting one takes the lock and serves.
    let mut served = false;
    for _ in 0..50 {
        if holder(dir.path()) == daemons[loser].id() && dir.path().join(paths::SOCKET).exists() {
            served = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(served, "the waiting daemon never took over");
    let (mut lines, mut w) = connect(dir.path()).await;
    call(&mut lines, &mut w, json!({"id": 3, "op": "shutdown"})).await;
    daemons[loser].wait().unwrap();
}
