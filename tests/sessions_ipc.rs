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

fn ended(e: &Env) -> Vec<Value> {
    let r = e.ask(json!({"id": 1, "op": "sessions"}));
    r["ended"].as_array().unwrap().clone()
}

fn by<'a>(sessions: &'a [Value], conversation: &str) -> Option<&'a Value> {
    sessions.iter().find(|s| s["conversation"] == conversation)
}

#[test]
fn the_listing_feeds_sessions_and_when_claude_fails_nothing_alive_ends() {
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
    // Read on its own thread: the first answers come from the rows alone. Then 8 live listed
    // rows, and the row registered just now, which so young a listing says nothing against.
    until(10, "the listing is read", || listed(&e).len() == 9);
    let s = listed(&e);
    let blocked = by(&s, "b1b2c3d4-0000-4000-8000-000000000002").unwrap();
    assert_eq!(
        (blocked["status"].as_str(), blocked["waiting_for"].as_str()),
        (Some("waiting"), Some("permission prompt"))
    );
    // A finished job whose process lives is idle, not ended; one with none is ended.
    let done = by(&s, "c1b2c3d4-0000-4000-8000-000000000003").unwrap();
    assert_eq!(done["job"]["state"], "done");
    assert!(by(&s, "d1b2c3d4-0000-4000-8000-000000000004").is_none());
    let over = ended(&e);
    assert!(by(&over, "c1b2c3d4-0000-4000-8000-000000000003").is_none());
    // The pid-less job started long before the window in this fixture, so it never shows.
    assert!(by(&over, "d1b2c3d4-0000-4000-8000-000000000004").is_none());
    assert!(by(&over, "mine").is_none());

    // Claude fails and there are no pid files here: nothing says these are over.
    let tries = shim.path().join("tries");
    set_shim(
        shim.path(),
        &format!("echo x >> '{}'\nexit 1", tries.display()),
    );
    until(15, "the failed read lands", || {
        register_claude(&e, "mine", "busy", None);
        // Listing them first: a look is what starts a read.
        listed(&e).len() == 9 && tries.exists()
    });
    let s = listed(&e);
    assert_eq!(by(&s, "mine").unwrap()["status"], "live");
    assert!(by(&ended(&e), "mine").is_none());

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

const LISTING: &str = "LIGHTBULB_CONNECT_DEBUG_LISTING";

/// A daemon asking a `claude` that prints `listing`.
fn listing_daemon(listing: &str) -> (Env, tempfile::TempDir) {
    let (shim, path) = claude_shim(listing);
    let e = Env::new();
    e.daemon(false, "", &[("PATH", path), (LISTING, "1".into())]);
    (e, shim)
}

/// A live interactive row of `pid`, as Claude lists it.
fn live_row(conversation: &str, pid: u32) -> Value {
    json!({"pid": pid, "cwd": "/tmp/p", "kind": "interactive", "startedAt": 1,
        "sessionId": conversation, "status": "busy"})
}

fn set_listing(shim: &tempfile::TempDir, rows: Value) {
    let (tmp, file) = (shim.path().join("new"), shim.path().join("listing.json"));
    std::fs::write(&tmp, rows.to_string()).unwrap();
    std::fs::rename(tmp, file).unwrap();
}

/// Every row's last word as `secs` older, in place of waiting for it.
fn age_rows(e: &Env, secs: i64) {
    let c = rusqlite::Connection::open(e.state.path().join("state.db")).unwrap();
    c.execute("UPDATE sessions SET seen_at = seen_at - ?1", [secs])
        .unwrap();
}

/// A hook event: the daemon reads the listing again within 2 s. A Codex row, which the listing
/// says nothing about.
fn poke(e: &Env) {
    let pid = std::process::id();
    let r = e.ask(
        json!({"id": 1, "op": "register", "session": {"provider": "codex", "conversation": "poke",
        "pid": pid, "birth_micros": os::process(pid).unwrap().birth_micros, "cwd": "/tmp/p"}}),
    );
    assert_eq!(r["ok"], true, "{r}");
}

#[test]
fn a_listed_session_that_disappears_becomes_an_ended_row_with_its_title() {
    let a = "a1b2c3d4-0000-4000-8000-00000000000a";
    let b = "a1b2c3d4-0000-4000-8000-00000000000b";
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let me = std::process::id();
    let (e, shim) = listing_daemon(&json!([live_row(a, me), live_row(b, child.id())]).to_string());
    let projects = e.home.path().join(".claude/projects/-tmp-p");
    std::fs::create_dir_all(&projects).unwrap();
    std::fs::write(
        projects.join(format!("{a}.jsonl")),
        "{\"type\":\"ai-title\",\"aiTitle\":\"Fix the list\"}\n",
    )
    .unwrap();
    until(10, "the listing is read", || listed(&e).len() == 2);
    // Every session seen has a row, so a handle: `who` needs no hook for it.
    assert!(by(&listed(&e), a).unwrap()["handle"].is_string());
    assert!(e
        .store()
        .holds(&format!("{}/claude:{b}", e.short()))
        .unwrap());

    child.kill().unwrap();
    child.wait().unwrap();
    set_listing(&shim, json!([]));
    age_rows(&e, 60);
    poke(&e);
    until(15, "both end", || ended(&e).len() == 2);
    assert!(by(&listed(&e), a).is_none());
    let over = ended(&e);
    // This test's process lives: the harness moved on.
    let moved = by(&over, a).unwrap();
    assert_eq!(moved["reason"], "moved-on");
    assert_eq!(moved["title"], "Fix the list");
    assert_eq!(moved["resume"], format!("claude --resume {a}"));
    assert!(moved["ended_at"].as_i64().unwrap() >= common::now_s() - 60);
    assert_eq!(by(&over, b).unwrap()["reason"], "process-exited");
}

/// `/clear`: the same process, on a new conversation in the pid file and the listing.
#[test]
fn a_live_pid_whose_conversation_left_the_listing_and_pid_file_ends_moved_on() {
    let (old, new) = (
        "a1b2c3d4-0000-4000-8000-00000000000a",
        "a1b2c3d4-0000-4000-8000-00000000000b",
    );
    let me = std::process::id();
    let (e, shim) = listing_daemon(&json!([live_row(old, me)]).to_string());
    let sessions = e.home.path().join(".claude/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let pid_file = |conversation: &str| {
        std::fs::write(
            sessions.join(format!("{me}.json")),
            json!({"pid": me, "sessionId": conversation, "cwd": "/tmp/p", "status": "shell",
                "startedAt": os::started_ms(me).unwrap()})
            .to_string(),
        )
        .unwrap();
    };
    pid_file(old);
    until(10, "the row is written", || {
        by(&listed(&e), old).is_some_and(|s| s["handle"].is_string())
    });
    pid_file(new);
    set_listing(&shim, json!([live_row(new, me)]));
    until(15, "the old one ends", || !ended(&e).is_empty());
    let over = ended(&e);
    assert_eq!(by(&over, old).unwrap()["reason"], "moved-on");
    assert_eq!(by(&listed(&e), new).unwrap()["status"], "live");
    assert!(by(&listed(&e), old).is_none());
}

/// No listing and no pid file say nothing against a live process; a dead one still ends.
#[test]
fn with_no_listing_and_no_pid_files_a_live_pid_ends_nothing() {
    let e = Env::new();
    e.daemon(false, "", &[]);
    register_claude(&e, "alive", "busy", None);
    let r = e.ask(
        json!({"id": 1, "op": "register", "session": {"provider": "claude",
        "conversation": "dead", "pid": 999_999, "birth_micros": 1, "cwd": "/tmp/p"}}),
    );
    assert_eq!(r["ok"], true, "{r}");
    for _ in 0..3 {
        assert!(by(&listed(&e), "alive").is_some());
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let over = ended(&e);
    assert_eq!(over.len(), 1, "{over:?}");
    assert_eq!(by(&over, "dead").unwrap()["reason"], "process-exited");
}

#[test]
fn a_finished_job_ends_once_and_is_not_reborn() {
    let j = "a1b2c3d4-0000-4000-8000-00000000000a";
    let job = json!([{"cwd": "/tmp/p", "kind": "background", "id": "ab12", "state": "done",
        "name": "the job", "startedAt": common::now_s() * 1000, "sessionId": j}]);
    let (e, _shim) = listing_daemon(&job.to_string());
    until(10, "the job is seen", || !ended(&e).is_empty());
    let first = ended(&e);
    let row = by(&first, j).unwrap();
    assert_eq!(row["reason"], "job-done");
    assert_eq!(row["cwd"], "/tmp/p");
    assert_eq!(row["title"], "the job");
    assert_eq!(row["resume"], format!("claude --resume {j}"));
    assert!(row["handle"].is_null(), "an old job uses up no number");
    assert!(by(&listed(&e), j).is_none());
    std::thread::sleep(std::time::Duration::from_secs(3));
    poke(&e);
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert_eq!(ended(&e), first);
}

/// Jobs Claude has listed for months arrive a read at a time, stay hidden past the window, and
/// are not deleted and born again while they are listed.
#[test]
fn a_finished_job_past_the_window_never_shows_and_is_not_recreated() {
    let started = common::now_s() - 8 * 86_400;
    let jobs: Vec<Value> = (0..201)
        .map(|i| {
            json!({"cwd": "/tmp/p", "kind": "background", "id": "ab12", "state": "done",
            "startedAt": started * 1000,
            "sessionId": format!("a1b2c3d4-0000-4000-8000-{i:012}")})
        })
        .collect();
    let (e, shim) = listing_daemon(&json!(jobs).to_string());
    let db = e.state.path().join("state.db");
    let rows = || {
        common::count(
            &db,
            "SELECT count(*) FROM sessions WHERE reason = 'job-done'",
        )
    };
    until(10, "the first read's jobs", || {
        assert!(ended(&e).is_empty());
        rows() > 0
    });
    assert_eq!(rows(), 200, "the first read's");
    assert_eq!(
        common::count(&db, "SELECT min(ended_at) FROM sessions"),
        started
    );
    until(15, "the next read's", || {
        assert!(ended(&e).is_empty());
        rows() == 201
    });
    // Still listed: kept, however often the ended list and the listing are read.
    poke(&e);
    for _ in 0..30 {
        assert!(ended(&e).is_empty());
        assert_eq!(rows(), 201);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // Off the listing it goes at the next look.
    set_listing(&shim, json!([]));
    poke(&e);
    until(15, "its rows go", || {
        ended(&e);
        rows() == 0
    });
}

/// As `register` refuses them: a session or a job under a hidden folder gets no row.
#[test]
fn a_hidden_folder_gets_no_row() {
    let j = "a1b2c3d4-0000-4000-8000-00000000000a";
    let e = Env::new();
    let off = e.home.path().canonicalize().unwrap().join("off");
    std::fs::create_dir_all(&off).unwrap();
    let cwd = off.to_str().unwrap();
    let me = std::process::id();
    let (_shim, path) = claude_shim(
        &json!([{"cwd": cwd, "kind": "background", "id": "ab12", "state": "done",
            "startedAt": common::now_s() * 1000, "sessionId": j}])
        .to_string(),
    );
    let sessions = e.home.path().join(".claude/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join("a.json"),
        json!({"pid": me, "sessionId": "seen", "cwd": cwd, "status": "busy",
            "startedAt": os::started_ms(me).unwrap()})
        .to_string(),
    )
    .unwrap();
    e.daemon(
        false,
        &format!("folders_off = [{cwd:?}]\n"),
        &[("PATH", path), (LISTING, "1".into())],
    );
    let db = e.state.path().join("state.db");
    // The `claude` is read within the first look; a later job would show by now.
    let reads = std::time::Instant::now();
    while reads.elapsed() < std::time::Duration::from_secs(2) {
        let s = listed(&e);
        assert_eq!(by(&s, "seen").unwrap()["hidden"], true);
        assert!(ended(&e).is_empty());
        assert_eq!(common::count(&db, "SELECT count(*) FROM sessions"), 0);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// A listing read before a row's last word has settled says nothing: a just-started
/// conversation or a just-fired hook may not be listed yet.
#[test]
fn a_fresh_row_absent_from_the_listing_stays_until_a_listing_15_s_later_lacks_it() {
    let (shim, path) = claude_shim("[]");
    let reads = shim.path().join("reads");
    set_shim(
        shim.path(),
        &format!("echo x >> '{}'\necho '[]'", reads.display()),
    );
    let e = Env::new();
    e.daemon(false, "", &[("PATH", path), (LISTING, "1".into())]);
    let read_count = || {
        let text = std::fs::read_to_string(&reads).unwrap_or_default();
        text.lines().count()
    };
    until(10, "the first read", || {
        listed(&e);
        read_count() >= 1
    });
    register_claude(&e, "fresh", "busy", None);
    assert_eq!(by(&listed(&e), "fresh").unwrap()["status"], "live");
    // A later read, within the 15 s, still lacks it: it stays.
    let before = read_count();
    until(10, "a read after the register", || {
        listed(&e);
        read_count() > before
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(by(&listed(&e), "fresh").is_some());
    assert!(ended(&e).is_empty());
    // Fifteen seconds of quiet later, one more read lacks it.
    age_rows(&e, 15);
    poke(&e);
    until(15, "it ends", || !ended(&e).is_empty());
    assert_eq!(by(&ended(&e), "fresh").unwrap()["reason"], "moved-on");
    assert!(by(&listed(&e), "fresh").is_none());
}

/// Ending does not wait for anyone to ask, with no teammate online either. Read from the store:
/// nothing here looks at `sessions`.
#[test]
fn a_session_ends_with_nobody_asking() {
    let c = "a1b2c3d4-0000-4000-8000-00000000000a";
    let (e, shim) = listing_daemon(&json!([live_row(c, std::process::id())]).to_string());
    register_claude(&e, c, "busy", None);
    set_listing(&shim, json!([]));
    age_rows(&e, 60);
    poke(&e);
    let db = e.state.path().join("state.db");
    until(15, "it ends", || {
        common::count(
            &db,
            "SELECT count(*) FROM sessions WHERE provider = 'claude' AND ended_at IS NOT NULL",
        ) == 1
    });
}
