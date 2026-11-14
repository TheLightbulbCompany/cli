//! Push into Codex: a thread served by Codex's app-server daemon gets
//! `<daemon codex> queue --thread <id> --message <framed>`; any other Codex thread is never pushed.
use lightbulb::{daemon, frame, os, paths};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

type Conn = (Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf);

async fn call(c: &mut Conn, req: Value) -> Value {
    c.1.write_all(format!("{req}\n").as_bytes()).await.unwrap();
    serde_json::from_str(&c.0.next_line().await.unwrap().unwrap()).unwrap()
}

/// One home for the whole binary (the env var is process-wide).
fn home() -> &'static Path {
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    HOME.get_or_init(|| {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("LIGHTBULB_CONNECT_CLAUDE_HOME", home.path());
        home
    })
    .path()
}

/// A fake `codex` at `<home>/<rel>/codex`. Call n records its argv (NUL-separated) in `argv.<n>`
/// and stdin in `stdin.<n>`, sleeps `delay` seconds if that file exists, exits 1 from call
/// `fail_from` on if that file exists, else answers as `codex queue` does.
fn fake_codex(rel: &str) -> PathBuf {
    let dir = home().join(rel);
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("codex");
    std::fs::write(
        &exe,
        "#!/bin/sh\nd=$(dirname \"$0\")\nn=$(( $(cat \"$d/count\" 2>/dev/null || echo 0) + 1 ))\n\
         echo $n > \"$d/count\"\nprintf '%s\\0' \"$@\" > \"$d/argv.$n\"\ncat > \"$d/stdin.$n\"\n\
         [ -f \"$d/delay\" ] && sleep \"$(cat \"$d/delay\")\"\n\
         [ -f \"$d/fail_from\" ] && [ \"$n\" -ge \"$(cat \"$d/fail_from\")\" ] && exit 1\n\
         echo \"Queued message 01a08ec5-b961-77c0-962d-25097fe0f5f4 for thread $3.\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    exe
}

/// Starts a daemon and registers this process as Codex thread `conv`, run by `exe`.
async fn start(conv: &str, exe: &Path) -> (tempfile::TempDir, Conn, String) {
    home();
    let dir = tempfile::tempdir().unwrap();
    // Network workers on: one test pushes an assignment.
    std::fs::write(dir.path().join("config.toml"), "network_workers = true\n").unwrap();
    let state = dir.path().to_path_buf();
    tokio::spawn(async move { daemon::run(&state).await.unwrap() });
    let sock = dir.path().join(paths::SOCKET);
    let mut stream = None;
    for _ in 0..50 {
        if let Ok(s) = UnixStream::connect(&sock).await {
            stream = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (r, w) = stream.expect("daemon socket never came up").into_split();
    let mut c = (BufReader::new(r).lines(), w);
    let birth = os::process(std::process::id()).unwrap().birth_micros;
    let reg = call(
        &mut c,
        json!({"id": 1, "op": "register", "session": {
        "provider": "codex", "conversation": conv, "pid": std::process::id(),
        "birth_micros": birth, "cwd": "/tmp", "executable": exe}}),
    )
    .await;
    let key = reg["key"]
        .as_str()
        .unwrap_or_else(|| panic!("{reg}"))
        .to_string();
    (dir, c, key)
}

async fn tier(c: &mut Conn) -> Value {
    call(c, json!({"id": 2, "op": "sessions"})).await["sessions"][0]["tier"].clone()
}

async fn send(c: &mut Conn, to: &str, text: &str, rid: &str) {
    let sent = call(
        c,
        json!({"id": 3, "op": "send", "from": "x/claude:a",
        "to": to, "text": text, "request_id": rid}),
    )
    .await;
    assert_eq!(sent["receipt"]["state"], "stored", "{sent}");
}

async fn state(c: &mut Conn, rid: &str) -> Value {
    call(
        c,
        json!({"id": 4, "op": "receipt", "from": "x/claude:a", "request_id": rid}),
    )
    .await["receipt"]["state"]
        .clone()
}

/// Waits (5 s at most) until `dir/count` reads `n`.
async fn calls(dir: &Path, n: u32) {
    for _ in 0..50 {
        let got = std::fs::read_to_string(dir.join("count")).unwrap_or_default();
        if got.trim() == n.to_string() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("fake codex was not called {n} times");
}

fn argv(dir: &Path, n: u32) -> Vec<Vec<u8>> {
    let raw = std::fs::read(dir.join(format!("argv.{n}"))).unwrap();
    raw.strip_suffix(b"\0")
        .unwrap()
        .split(|b| *b == 0)
        .map(Vec::from)
        .collect()
}

fn expected_argv(thread: &str, framed: &str) -> Vec<Vec<u8>> {
    ["queue", "--thread", thread, "--message", framed]
        .map(|a| a.as_bytes().to_vec())
        .to_vec()
}

#[tokio::test]
async fn codex_push_runs_codex_queue_with_the_thread_id() {
    let exe = fake_codex(".codex/packages/app-server-daemon/releases/ok/bin");
    let bin = exe.parent().unwrap();
    let (_dir, mut c, to) = start("t1", &exe).await;
    assert_eq!(tier(&mut c).await, "push");

    send(&mut c, &to, "hello codex", "r1").await;
    calls(bin, 1).await;
    let mut s = Value::Null;
    for _ in 0..50 {
        s = state(&mut c, "r1").await;
        if s == "delivered" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(s, "delivered");
    let framed = frame::framed("Your Claude Code", "r1", "hello codex");
    assert_eq!(argv(bin, 1), expected_argv("t1", &framed));
    assert_eq!(std::fs::read(bin.join("stdin.1")).unwrap(), b"");
    assert_eq!(
        call(&mut c, json!({"id": 5, "op": "unread", "key": to})).await["unread"],
        0
    );
}

/// An assignment is pushed with its own header: its done_when and deadline, and how to answer.
#[tokio::test]
async fn codex_push_frames_an_assignment_as_one() {
    let exe = fake_codex(".codex/packages/app-server-daemon/releases/assign/bin");
    let bin = exe.parent().unwrap();
    let (_dir, mut c, to) = start("t4", &exe).await;
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    };
    let before = now();
    let sent = call(
        &mut c,
        json!({"id": 3, "op": "send", "from": "x/claude:a", "to": to, "kind": "assign",
        "text": "run the tests", "done_when": "tests pass", "deadline_s": 600,
        "request_id": "as1"}),
    )
    .await;
    let after = now();
    assert_eq!(sent["receipt"]["state"], "stored", "{sent}");
    calls(bin, 1).await;
    let got = argv(bin, 1);
    let want = (before..=after).map(|t| {
        let meta = lightbulb::mail::Meta {
            done_when: Some("tests pass".into()),
            deadline_at: Some(t + 600),
            ..Default::default()
        };
        let framed = frame::framed_kind(
            "assign",
            "Your Claude Code",
            "as1",
            None,
            &meta,
            "run the tests",
        );
        expected_argv("t4", &framed)
    });
    assert!(
        want.into_iter().any(|w| w == got),
        "{:?}",
        String::from_utf8_lossy(&got[4])
    );
}

#[tokio::test]
async fn a_codex_thread_outside_the_daemon_is_next_step_and_never_pushed() {
    let exe = fake_codex("elsewhere/bin");
    let (_dir, mut c, to) = start("t2", &exe).await;
    assert_eq!(tier(&mut c).await, "next_step");

    send(&mut c, &to, "wait for the hook", "r2").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!exe.parent().unwrap().join("count").exists(), "nothing ran");
    assert_eq!(state(&mut c, "r2").await, "stored");
}

#[tokio::test]
async fn a_partial_batch_delivers_only_what_was_pushed() {
    let exe = fake_codex(".codex/packages/app-server-daemon/releases/batch/bin");
    let bin = exe.parent().unwrap();
    std::fs::write(bin.join("delay"), "0.5").unwrap();
    std::fs::write(bin.join("fail_from"), "2").unwrap();
    let (_dir, mut c, to) = start("t3", &exe).await;

    // r2 arrives while r1's push is in flight: r1 goes through, r2's push fails (call 2 from
    // the first delivery, call 3 from the second).
    send(&mut c, &to, "first", "r1").await;
    send(&mut c, &to, "second", "r2").await;
    calls(bin, 3).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(state(&mut c, "r1").await, "delivered");
    assert_eq!(state(&mut c, "r2").await, "stored");
    assert_eq!(
        call(&mut c, json!({"id": 5, "op": "unread", "key": to})).await["unread"],
        1
    );
    let first = frame::framed("Your Claude Code", "r1", "first");
    let second = frame::framed("Your Claude Code", "r2", "second");
    assert_eq!(argv(bin, 1), expected_argv("t3", &first));
    assert_eq!(argv(bin, 2), expected_argv("t3", &second));
    assert_eq!(
        tier(&mut c).await,
        "next_step",
        "a failed push reads next_step"
    );
}
