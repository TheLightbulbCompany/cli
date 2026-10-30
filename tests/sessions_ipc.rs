mod common;

use common::{until, Env};
use lightbulb::{daemon, os, paths};
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
    std::env::set_var("LIGHTBULB_CONNECT_CLAUDE_HOME", claude_home.path());
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
    assert_eq!(sessions[0]["tier"], "next_step");
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

#[test]
fn register_takes_waiting_with_what_for_and_end_takes_a_reason() {
    let e = Env::new();
    e.daemon(false, "", &[]);
    let pid = std::process::id();
    let birth = os::process(pid).unwrap().birth_micros;
    let r = e.ask(
        json!({"id": 1, "op": "register", "session": {"provider": "claude",
        "conversation": "w1", "pid": pid, "birth_micros": birth, "cwd": "/tmp/p",
        "status": "waiting", "waiting_for": "permission prompt"}}),
    );
    assert_eq!(r["ok"], true, "{r}");
    let s = &e.ask(json!({"id": 1, "op": "sessions"}))["sessions"][0];
    assert_eq!(
        (s["status"].as_str(), s["waiting_for"].as_str()),
        (Some("waiting"), Some("permission prompt"))
    );
    let r = e.ask(
        json!({"id": 1, "op": "end", "key": s["key"], "if_gone": false, "reason": "moved-on"}),
    );
    assert_eq!(r["ended"], true, "{r}");
    let r = e.ask(
        json!({"id": 1, "op": "register", "session": {"provider": "claude",
        "conversation": "w2", "pid": pid, "birth_micros": birth, "cwd": "/tmp/p",
        "status": "waiting", "waiting_for": "x".repeat(65)}}),
    );
    assert_eq!(r["error"], "bad_request", "{r}");
    // Exactly 64 is accepted.
    let r = e.ask(
        json!({"id": 1, "op": "register", "session": {"provider": "claude",
        "conversation": "w3", "pid": pid, "birth_micros": birth, "cwd": "/tmp/p",
        "status": "waiting", "waiting_for": "x".repeat(64)}}),
    );
    assert_eq!(r["ok"], true, "{r}");
    let key = r["key"].clone();
    let r = e.ask(json!({"id": 1, "op": "end", "key": key, "reason": "job-failed"}));
    assert_eq!(r["error"], "bad_request", "{r}");
    let r = e.ask(json!({"id": 1, "op": "end", "key": key, "reason": "session-end"}));
    assert_eq!(r["ended"], true, "{r}");
}

/// A `claude` on a fresh dir first on `PATH`, printing `listing`; keep the dir alive while in
/// use. Returns it with the `PATH` to give the daemon (launchd's minimal one after it).
fn claude_shim(listing: &str) -> (tempfile::TempDir, String) {
    let shim = tempfile::tempdir().unwrap();
    std::fs::write(shim.path().join("listing.json"), listing).unwrap();
    set_shim(
        shim.path(),
        &format!("cat '{}'", shim.path().join("listing.json").display()),
    );
    let path = format!("{}:/usr/bin:/bin", shim.path().display());
    (shim, path)
}

fn set_shim(dir: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("claude");
    let tmp = dir.join("claude.new");
    std::fs::write(&tmp, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(tmp, bin).unwrap();
}

fn register_claude(e: &Env, conversation: &str, status: &str, waiting_for: Option<&str>) {
    let pid = std::process::id();
    let birth = os::process(pid).unwrap().birth_micros;
    let r = e.ask(
        json!({"id": 1, "op": "register", "session": {"provider": "claude",
        "conversation": conversation, "pid": pid, "birth_micros": birth, "cwd": "/tmp/p",
        "status": status, "waiting_for": waiting_for}}),
    );
    assert_eq!(r["ok"], true, "{r}");
}

fn listed(e: &Env) -> Vec<Value> {
    let r = e.ask(json!({"id": 1, "op": "sessions"}));
    r["sessions"].as_array().unwrap().clone()
}

fn by<'a>(sessions: &'a [Value], conversation: &str) -> Option<&'a Value> {
    sessions.iter().find(|s| s["conversation"] == conversation)
}

#[test]
fn the_listing_feeds_sessions_and_when_claude_fails_only_the_registered_row_stays() {
    let five = include_str!("fixtures/claude-agents/five.json")
        .replace("\"pid\": 4242", &format!("\"pid\": {}", std::process::id()));
    let (shim, path) = claude_shim(&five);
    let e = Env::new();
    e.daemon(
        false,
        "",
        &[
            ("PATH", path),
            ("LIGHTBULB_CONNECT_DEBUG_LISTING", "1".into()),
        ],
    );
    register_claude(&e, "mine", "idle", None);
    // Read on its own thread: the first answers come from the rows alone.
    until(10, "the listing is read", || listed(&e).len() == 9);
    let s = listed(&e);
    let blocked = by(&s, "b1b2c3d4-0000-4000-8000-000000000002").unwrap();
    assert_eq!(
        (blocked["status"].as_str(), blocked["waiting_for"].as_str()),
        (Some("waiting"), Some("permission prompt"))
    );
    let done = by(&s, "c1b2c3d4-0000-4000-8000-000000000003").unwrap();
    assert_eq!(done["job"]["state"], "done");
    assert!(by(&s, "d1b2c3d4-0000-4000-8000-000000000004").is_none());

    // Claude fails, and there are no pid files here: the registered row is the whole list.
    let tries = shim.path().join("tries");
    set_shim(
        shim.path(),
        &format!("echo x >> '{}'\nexit 1", tries.display()),
    );
    register_claude(&e, "mine", "busy", None);
    until(15, "the failed read lands", || listed(&e).len() == 1);
    let s = listed(&e);
    assert_eq!(s[0]["conversation"], "mine");
    assert_eq!(s[0]["status"], "live");

    // One line in the daemon's log for the streak, however many reads fail.
    let count = || {
        std::fs::read_to_string(&tries)
            .unwrap_or_default()
            .lines()
            .count()
    };
    until(15, "a second failed read", || {
        register_claude(&e, "mine", "busy", None);
        listed(&e);
        count() >= 2
    });
    std::thread::sleep(std::time::Duration::from_millis(200));
    let log = std::fs::read_to_string(e.state.path().join("daemon.log")).unwrap();
    assert_eq!(log.matches("claude agents: no answer").count(), 1, "{log}");
}

/// A hook's `waiting` is newer than a listing that still says `busy`; the next hook moves it
/// on, and a later listing saying `idle` clears a new `waiting`.
#[test]
fn a_hook_waiting_stands_over_a_stale_listing() {
    let c = "e1b2c3d4-0000-4000-8000-000000000005";
    let row = |status: &str| {
        json!([{"pid": std::process::id(), "cwd": "/tmp/p", "kind": "interactive", "name": "shim",
            "startedAt": 1, "sessionId": c, "status": status}])
        .to_string()
    };
    let (shim, path) = claude_shim(&row("busy"));
    let e = Env::new();
    e.daemon(
        false,
        "",
        &[
            ("PATH", path),
            ("LIGHTBULB_CONNECT_DEBUG_LISTING", "1".into()),
        ],
    );
    register_claude(&e, c, "waiting", Some("permission prompt"));
    // The listing's `name` shows it has been read.
    until(10, "the listing is read", || {
        by(&listed(&e), c).is_some_and(|s| s["title"] == "shim")
    });
    let s = listed(&e);
    let w = by(&s, c).unwrap();
    assert_eq!(
        (w["status"].as_str(), w["waiting_for"].as_str()),
        (Some("waiting"), Some("permission prompt"))
    );
    register_claude(&e, c, "busy", None);
    assert_eq!(by(&listed(&e), c).unwrap()["status"], "live");

    std::fs::write(shim.path().join("listing.json"), row("idle")).unwrap();
    register_claude(&e, c, "waiting", Some("permission prompt"));
    until(15, "the listing's idle clears it", || {
        by(&listed(&e), c).is_some_and(|s| s["status"] == "idle")
    });
}

/// Only the listing clears a hook's `waiting`: a pid file saying `idle` may just be behind it.
#[test]
fn a_pid_file_saying_idle_leaves_a_hook_waiting() {
    let e = Env::new();
    let sessions = e.home.path().join(".claude/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let pid = std::process::id();
    std::fs::write(
        sessions.join("a.json"),
        json!({"pid": pid, "sessionId": "w", "cwd": "/tmp/p", "status": "idle",
            "startedAt": os::started_ms(pid).unwrap()})
        .to_string(),
    )
    .unwrap();
    e.daemon(false, "", &[]);
    register_claude(&e, "w", "waiting", Some("input needed"));
    assert_eq!(by(&listed(&e), "w").unwrap()["status"], "waiting");
}

/// A test daemon whose `PATH` has a `claude` leaves it alone unless told to ask it.
#[test]
fn a_debug_daemon_never_runs_a_claude_it_was_not_told_to() {
    let shim = tempfile::tempdir().unwrap();
    let ran = shim.path().join("ran");
    set_shim(
        shim.path(),
        &format!("touch '{}'\necho '[]'", ran.display()),
    );
    let e = Env::new();
    let path = format!("{}:/usr/bin:/bin", shim.path().display());
    e.daemon(false, "", &[("PATH", path)]);
    register_claude(&e, "mine", "idle", None);
    assert_eq!(listed(&e).len(), 1);
    assert!(!ran.exists());
}

/// The refresh runs on its own thread: a `claude` that hangs delays no caller.
#[test]
fn a_hanging_claude_delays_neither_sessions_nor_register() {
    let (shim, path) = claude_shim("[]");
    set_shim(shim.path(), "exec sleep 10");
    let e = Env::new();
    e.daemon(
        false,
        "",
        &[
            ("PATH", path),
            ("LIGHTBULB_CONNECT_DEBUG_LISTING", "1".into()),
        ],
    );
    let quick = |what: &str, f: &dyn Fn()| {
        let t = std::time::Instant::now();
        f();
        assert!(
            t.elapsed() < std::time::Duration::from_millis(900),
            "{what}: {:?}",
            t.elapsed()
        );
    };
    quick("the first sessions", &|| {
        listed(&e);
    });
    quick("register", &|| register_claude(&e, "mine", "busy", None));
    quick("sessions", &|| assert_eq!(listed(&e).len(), 1));
}
