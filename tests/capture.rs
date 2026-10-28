//! Local capture: sessions and tool calls the hooks send to the daemon, which writes them.

mod common;

use common::{err, out, Env, EXE};
use lightbulb::capture;
use lightbulb::os;
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

fn store() -> (tempfile::TempDir, lightbulb::store::Store) {
    let d = tempfile::tempdir().unwrap();
    let s = lightbulb::store::Store::open(&d.path().join("state.db")).unwrap();
    (d, s)
}

/// A real-shaped Claude Code transcript: two replies (one written as two entries), 120 tokens in
/// and 45 out.
fn transcript() -> String {
    format!(
        "{}/tests/fixtures/hooks/claude-transcript.jsonl",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn row() -> capture::SessionRow {
    capture::SessionRow {
        id: "a:1".into(),
        harness: "a".into(),
        computer: "mac".into(),
        repo: None,
        branch: None,
        started: 1,
        ended: None,
        prompt_tokens: None,
        completion_tokens: None,
    }
}

impl Env {
    /// A folder under this env's HOME (resolved, as a hook reports it).
    fn folder(&self, rel: &str) -> PathBuf {
        let p = self.home.path().canonicalize().unwrap().join(rel);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// `hook claude <arg>` as Claude Code runs it, called as this test process.
    fn claude_hook(&self, arg: &str, payload: Value) -> Output {
        self.claude_hook_env(arg, payload, &[])
    }

    fn claude_hook_env(&self, arg: &str, payload: Value, env: &[(&str, &str)]) -> Output {
        let me = os::process(std::process::id()).unwrap();
        let mut child = self
            .base_cmd(Command::new(EXE))
            .args(["hook", "claude", arg])
            .envs(env.iter().copied())
            .env(
                "LIGHTBULB_CONNECT_HOOK_CALLER",
                format!("{}:{}", me.pid, me.birth_micros),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn payload(&self, event: &str, cwd: &PathBuf, extra: Value) -> Value {
        let mut p = json!({"session_id": "c1", "cwd": cwd, "hook_event_name": event});
        p.as_object_mut()
            .unwrap()
            .extend(extra.as_object().cloned().unwrap());
        p
    }

    /// A full session in `cwd`: start, one `ls`, a stop that reports tokens, end.
    fn run_session(&self, cwd: &PathBuf) {
        let steps = [
            ("session-start", "SessionStart", json!({})),
            (
                "post-tool-use",
                "PostToolUse",
                json!({"tool_name": "Bash", "tool_input": {"command": "ls"},
                    "tool_response": {"stdout": "secret output"}}),
            ),
            ("stop", "Stop", json!({"transcript_path": transcript()})),
            (
                "session-end",
                "SessionEnd",
                json!({"transcript_path": transcript()}),
            ),
        ];
        for (arg, event, extra) in steps {
            let o = self.claude_hook(arg, self.payload(event, cwd, extra));
            assert!(o.status.success(), "{}", err(&o));
        }
    }

    /// The one captured session, once the daemon has written what `done` waits for (capture
    /// does not wait for the write).
    fn captured(
        &self,
        what: &str,
        done: impl Fn(&capture::SessionRow) -> bool,
    ) -> capture::SessionRow {
        let mut row = None;
        common::until(10, what, || {
            row = capture::sessions_since(&self.store(), -1).unwrap().pop();
            row.as_ref().is_some_and(&done)
        });
        row.unwrap()
    }

    fn sessions(&self, args: &[&str]) -> Output {
        let mut argv = vec![EXE, "sessions"];
        argv.extend(args);
        self.detached(&argv, "")
    }
}

#[test]
fn session_rows_follow_hook_lifecycle() {
    let e = Env::new();
    e.daemon(false, "", &[]);
    let cwd = e.folder("proj");
    e.run_session(&cwd);
    let s = &e.captured("the session ends", |s| s.ended.is_some());
    let db = e.store();
    assert_eq!(capture::sessions_since(&db, -1).unwrap().len(), 1);
    assert_eq!((s.id.as_str(), s.harness.as_str()), ("claude:c1", "claude"));
    assert_eq!(
        (s.prompt_tokens, s.completion_tokens),
        (Some(120), Some(45))
    );
    let calls = capture::tool_calls(&db, &s.id, true).unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        (
            calls[0].seq,
            calls[0].tool.as_str(),
            calls[0].summary.as_deref()
        ),
        (1, "Bash", Some("Bash: ls"))
    );
    // Output is never kept.
    assert!(!format!("{calls:?}").contains("secret output"));
}

#[test]
fn a_stop_without_usage_leaves_tokens_null() {
    let e = Env::new();
    e.daemon(false, "", &[]);
    let cwd = e.folder("proj");
    e.claude_hook("session-start", e.payload("SessionStart", &cwd, json!({})));
    e.claude_hook("stop", e.payload("Stop", &cwd, json!({})));
    std::thread::sleep(std::time::Duration::from_millis(200));
    let s = e.captured("the session", |_| true);
    assert_eq!((s.prompt_tokens, s.completion_tokens), (None, None));
}

/// Tokens come from the transcript at SessionEnd only: a Stop, every turn, never reads it.
#[test]
fn tokens_are_read_at_session_end_never_at_stop() {
    let e = Env::new();
    e.daemon(false, "", &[]);
    let cwd = e.folder("proj");
    // A transcript just under the cap: parsing it takes far longer than a hook may.
    let fixture = std::fs::read_to_string(transcript()).unwrap();
    let big = e.home.path().join("big.jsonl");
    let times = capture::TRANSCRIPT_MAX as usize / fixture.len();
    std::fs::write(&big, fixture.repeat(times)).unwrap();
    let path = json!({ "transcript_path": big });
    e.claude_hook("session-start", e.payload("SessionStart", &cwd, json!({})));
    e.captured("the session starts", |_| true);
    let t = std::time::Instant::now();
    let o = e.claude_hook("stop", e.payload("Stop", &cwd, path.clone()));
    let took = t.elapsed();
    assert!(o.status.success(), "{}", err(&o));
    assert!(took < std::time::Duration::from_millis(1000), "{took:?}");
    std::thread::sleep(std::time::Duration::from_millis(200));
    let s = e.captured("the session", |_| true);
    assert_eq!((s.prompt_tokens, s.completion_tokens), (None, None));
    // A Stop whose own payload carries usage is still read.
    let usage = json!({"usage": {"input_tokens": 7, "output_tokens": 3}});
    e.claude_hook("stop", e.payload("Stop", &cwd, usage));
    e.captured("usage from the payload", |s| s.prompt_tokens == Some(7));
    let o = e.claude_hook("session-end", e.payload("SessionEnd", &cwd, path));
    assert!(o.status.success(), "{}", err(&o));
    let s = e.captured("tokens at the end", |s| s.ended.is_some());
    assert_eq!(
        (s.prompt_tokens, s.completion_tokens),
        (Some(120), Some(45))
    );
}

/// A daemon slow to answer a capture write does not hold the hook: it waits about 10 ms, says
/// so once, and the write still lands.
#[test]
fn a_slow_capture_write_does_not_hold_the_hook() {
    let e = Env::new();
    let delay = [(
        "LIGHTBULB_CONNECT_DEBUG_CAPTURE_DELAY_MS",
        "500".to_string(),
    )];
    e.daemon(false, "", &delay);
    let cwd = e.folder("proj");
    e.claude_hook("session-start", e.payload("SessionStart", &cwd, json!({})));
    e.captured("the session starts", |_| true);
    let post = json!({"tool_name": "Bash", "tool_input": {"command": "ls"}});
    let t = std::time::Instant::now();
    let o = e.claude_hook("post-tool-use", e.payload("PostToolUse", &cwd, post));
    let took = t.elapsed();
    assert!(o.status.success() && o.stdout.is_empty(), "{}", err(&o));
    assert!(took < std::time::Duration::from_millis(450), "{took:?}");
    let log = err(&o);
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(
        log.starts_with("lightbulb hook claude post-tool-use: capture"),
        "{log}"
    );
    common::until(5, "the tool call is written", || {
        capture::tool_calls(&e.store(), "claude:c1", true)
            .unwrap()
            .len()
            == 1
    });
}

#[test]
fn test_capture_refuses_secret_in_summary() {
    let (_d, s) = store();
    assert!(capture::start_session(&s, &row(), "/tmp/proj").unwrap());
    let secret = json!({"command": "export API_KEY=abcd1234efgh5678"}).to_string();
    assert!(capture::record_tool_call(&s, "a:1", "Bash", &secret, Some(5), true).unwrap());
    let plain = json!({"command": "ls"}).to_string();
    capture::record_tool_call(&s, "a:1", "Bash", &plain, None, false).unwrap();
    let calls = capture::tool_calls(&s, "a:1", true).unwrap();
    // The call is kept; only its summary is replaced, with the reason.
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].summary.as_deref(), Some("[refused: credential]"));
    assert_eq!(calls[0].duration_ms, Some(5));
    assert_eq!(calls[1].summary.as_deref(), Some("Bash: ls"));
    assert!(!calls[1].ok);
    // Without summaries none is returned.
    assert!(capture::tool_calls(&s, "a:1", false)
        .unwrap()
        .iter()
        .all(|c| c.summary.is_none()));
    // Long summaries are cut.
    let long = json!({"command": "x".repeat(500)}).to_string();
    capture::record_tool_call(&s, "a:1", "Bash", &long, None, true).unwrap();
    let last = capture::tool_calls(&s, "a:1", true).unwrap().pop().unwrap();
    assert_eq!(last.summary.unwrap().chars().count(), capture::SUMMARY_MAX);
}

#[test]
fn hidden_folder_is_not_captured() {
    let (_d, s) = store();
    // Only the first folder below HOME counts: `~/.config` is skipped, a repo's own dot
    // folders (Claude Code's worktrees) are captured.
    let home = std::env::var("HOME").unwrap();
    assert!(capture::hidden_folder(&format!("{home}/.config/x")));
    assert!(capture::hidden_folder(&format!("{home}/.cache")));
    assert!(!capture::hidden_folder(&format!(
        "{home}/code/repo/.claude/worktrees/x"
    )));
    assert!(!capture::hidden_folder("/tmp/.secret/proj"));
    let hidden = format!("{home}/.secret/proj");
    assert!(!capture::start_session(&s, &row(), &hidden).unwrap());
    // No row, so a tool call from that session is dropped too.
    assert!(!capture::record_tool_call(&s, "a:1", "Bash", "{}", None, true).unwrap());
    assert!(capture::sessions_since(&s, -1).unwrap().is_empty());
    assert!(capture::tool_calls(&s, "a:1", true).unwrap().is_empty());

    // End to end: a dot folder under HOME, and a folder turned off with `folder off`.
    let e = Env::new();
    let off = e.folder("private");
    let config = format!("folders_off = [{:?}]\n", off.to_string_lossy());
    e.daemon(false, &config, &[]);
    for cwd in [e.folder(".hidden/proj"), off.join("proj")] {
        std::fs::create_dir_all(&cwd).unwrap();
        e.run_session(&cwd);
    }
    // Capture does not wait for the write: give a wrong one time to land.
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(capture::sessions_since(&e.store(), -1).unwrap().is_empty());
    // A worktree under a repo's `.claude` folder is captured.
    e.run_session(&e.folder("code/repo/.claude/worktrees/x"));
    e.captured("the worktree session", |s| s.ended.is_some());
}

/// A claude session in `rel` with a message waiting for it, then its PostToolUse hook (the
/// one that hands mail over, and records the tool call), with capture's connection dead when
/// `down`.
fn mail_run(rel: &str, down: bool) -> (Env, Output) {
    let e = Env::new();
    e.daemon(false, "", &[]);
    let cwd = e.folder(rel);
    let o = e.claude_hook("session-start", e.payload("SessionStart", &cwd, json!({})));
    assert!(o.status.success(), "{}", err(&o));
    let to = format!("{}/claude:c1", e.short());
    let r = e.ask(json!({"id": 1, "op": "send", "from": "x/codex:a", "to": to,
        "text": "hello from a teammate", "request_id": "r1"}));
    assert_eq!(r["receipt"]["state"], "stored", "{r}");
    let env: &[(&str, &str)] = if down {
        &[("LIGHTBULB_CONNECT_DEBUG_CAPTURE_DOWN", "1")]
    } else {
        &[]
    };
    let post = json!({"tool_name": "Bash", "tool_input": {"command": "ls"}});
    let o = e.claude_hook_env("post-tool-use", e.payload("PostToolUse", &cwd, post), env);
    (e, o)
}

/// What a hook prints must not depend on capture: with its daemon connection dead, or with
/// the daemon refusing the session, the mail the hook hands over is byte-identical to a run
/// where capture worked, and nothing but the hook's own error line reaches stderr.
/// Removing `capture()` from the hook fails the "capture worked" assertions on the baseline
/// (no tool-call row); the silence assertions below are about what capture must not change.
#[test]
fn test_capture_write_failure_is_silent() {
    let (up_env, up) = mail_run("proj", false);
    // Nothing on stderr but, on a loaded machine, capture's one "not waiting" line.
    assert!(up.status.success(), "{}", err(&up));
    assert!(
        err(&up)
            .lines()
            .all(|l| l.starts_with("lightbulb hook claude post-tool-use: capture")),
        "{}",
        err(&up)
    );
    assert!(out(&up).contains("hello from a teammate"), "{}", out(&up));
    common::until(5, "capture worked on the baseline", || {
        capture::tool_calls(&up_env.store(), "claude:c1", true)
            .unwrap()
            .len()
            == 1
    });

    let silent = |o: &Output, what: &str| {
        assert!(o.status.success(), "{what}");
        assert_eq!(o.stdout, up.stdout, "{what}: the hook's output changed");
        let e = String::from_utf8_lossy(&o.stderr).to_string();
        assert!(
            e.lines()
                .all(|l| l.starts_with("lightbulb hook claude post-tool-use: ")),
            "{what}: {e}"
        );
    };

    // The daemon connection capture uses is dead.
    let (e, down) = mail_run("proj", true);
    silent(&down, "daemon down");
    assert!(capture::tool_calls(&e.store(), "claude:c1", true)
        .unwrap()
        .is_empty());

    // The daemon is up and its store refuses the session (a hidden folder): no row.
    let (e, refused) = mail_run(".hidden/proj", false);
    silent(&refused, "store refuses");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(capture::sessions_since(&e.store(), -1).unwrap().is_empty());
    assert!(capture::tool_calls(&e.store(), "claude:c1", true)
        .unwrap()
        .is_empty());

    // A hook run where not even the daemon can start still exits 0 with nothing on stdout.
    let e = Env::new();
    let long = e.home.path().join("a".repeat(120));
    std::fs::create_dir_all(&long).unwrap();
    let me = os::process(std::process::id()).unwrap();
    for (arg, event) in [
        ("post-tool-use", "PostToolUse"),
        ("session-end", "SessionEnd"),
    ] {
        let cwd = e.folder("proj");
        let p = e.payload(event, &cwd, json!({"tool_name": "Bash"}));
        let mut child = e
            .base_cmd(Command::new(EXE))
            .args(["hook", "claude", arg])
            .env("LIGHTBULB_CONNECT_HOME", &long)
            .env(
                "LIGHTBULB_CONNECT_HOOK_CALLER",
                format!("{}:{}", me.pid, me.birth_micros),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(p.to_string().as_bytes())
            .unwrap();
        let o = child.wait_with_output().unwrap();
        assert!(o.status.success() && o.stdout.is_empty(), "{arg}");
    }
    // Garbage on stdin, with a live daemon.
    e.daemon(false, "", &[]);
    let o = e.claude_hook("post-tool-use", json!("not an object"));
    assert!(o.status.success() && o.stdout.is_empty());
}

/// A session that was running before capture existed has no row; its next tool call opens one,
/// unless its folder is hidden.
#[test]
fn a_running_session_is_captured_from_its_next_tool_call() {
    let e = Env::new();
    e.daemon(false, "", &[]);
    let cwd = e.folder("proj");
    let post = json!({"tool_name": "Read", "tool_input": {"file_path": "/x/y.rs"}});
    e.claude_hook("post-tool-use", e.payload("PostToolUse", &cwd, post));
    let mut calls = vec![];
    common::until(10, "the call is recorded", || {
        calls = capture::tool_calls(&e.store(), "claude:c1", true).unwrap();
        !calls.is_empty()
    });
    assert_eq!(capture::sessions_since(&e.store(), -1).unwrap().len(), 1);
    assert_eq!(calls[0].summary.as_deref(), Some("Read: /x/y.rs"));
}

#[test]
fn an_ambiguous_session_prefix_is_named() {
    let e = Env::new();
    e.daemon(false, "", &[]);
    let cwd = e.folder("proj");
    for id in ["c1", "c2"] {
        let mut p = e.payload("SessionStart", &cwd, json!({}));
        p["session_id"] = id.into();
        e.claude_hook("session-start", p);
    }
    common::until(5, "both sessions start", || {
        capture::sessions_since(&e.store(), -1).unwrap().len() == 2
    });
    let o = e.sessions(&["show", "claude:c"]);
    assert!(!o.status.success());
    assert!(
        err(&o).contains("More than one session starts with that id"),
        "{}",
        err(&o)
    );
    let none = e.sessions(&["show", "nope"]);
    assert!(err(&none).contains("No session starts"), "{}", err(&none));
    assert!(e.sessions(&["show", "claude:c1"]).status.success());
}

#[test]
fn sessions_list_shows_harness_repo_tokens() {
    let e = Env::new();
    e.daemon(false, "", &[]);
    let cwd = e.folder("proj");
    std::fs::create_dir_all(cwd.join(".git")).unwrap();
    std::fs::write(
        cwd.join(".git/config"),
        "[remote \"origin\"]\n\turl = git@github.com:acme/app.git\n",
    )
    .unwrap();
    std::fs::write(cwd.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    assert_eq!(
        out(&e.sessions(&["list"])).trim(),
        "No sessions captured yet."
    );
    e.run_session(&cwd);
    e.captured("the session ends", |s| s.ended.is_some());

    let list = out(&e.sessions(&["list"]));
    let header: Vec<&str> = list.lines().next().unwrap().split_whitespace().collect();
    assert_eq!(header, ["Id", "Agent", "Repo", "Started", "In", "Out"]);
    let row = list.lines().nth(1).unwrap();
    assert!(
        row.contains("claude:c1") && row.contains("claude  acme/app "),
        "{row}"
    );
    assert!(row.ends_with("120  45"), "{row}");

    let show = out(&e.sessions(&["show", "claude:c1"]));
    assert!(show.contains("Bash: ls") && show.contains("main"), "{show}");
    let exported: Value = serde_json::from_str(&out(&e.sessions(&["export"]))).unwrap();
    assert_eq!(exported[0]["tool_calls"][0]["summary"], "Bash: ls");

    let o = e.sessions(&["forget", "claude:c1"]);
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(
        out(&e.sessions(&["list"])).trim(),
        "No sessions captured yet."
    );
    assert!(capture::tool_calls(&e.store(), "claude:c1", true)
        .unwrap()
        .is_empty());
    assert!(!e.sessions(&["show", "claude:c1"]).status.success());
}

#[test]
fn an_oversized_transcript_yields_null_tokens() {
    let d = tempfile::tempdir().unwrap();
    let line = std::fs::read_to_string(transcript()).unwrap();
    assert_eq!(capture::transcript_tokens(&transcript()), Some((120, 45)));
    // Longer than the cap (a sparse file): no total rather than a partial one.
    let big = d.path().join("big.jsonl");
    std::fs::write(&big, &line).unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(&big).unwrap();
    f.set_len(capture::TRANSCRIPT_MAX + 1).unwrap();
    assert_eq!(capture::transcript_tokens(big.to_str().unwrap()), None);
    // A line that is not UTF-8 ends the read early: no total.
    let bad = d.path().join("bad.jsonl");
    let mut bytes = line.into_bytes();
    bytes.extend_from_slice(b"\xff\xfe\n");
    std::fs::write(&bad, bytes).unwrap();
    assert_eq!(capture::transcript_tokens(bad.to_str().unwrap()), None);
    // Not a regular file.
    assert_eq!(capture::transcript_tokens("/dev/null"), None);
}

#[test]
fn forget_deletes_only_the_matching_session() {
    let (_d, s) = store();
    for id in ["a:1", "a:2"] {
        let mut r = row();
        r.id = id.into();
        capture::start_session(&s, &r, "/tmp/p").unwrap();
        capture::record_tool_call(&s, id, "Bash", "{}", None, true).unwrap();
    }
    assert!(capture::forget(&s, "a:1").unwrap());
    assert!(capture::tool_calls(&s, "a:1", true).unwrap().is_empty());
    assert_eq!(capture::sessions_since(&s, -1).unwrap().len(), 1);
    assert_eq!(capture::tool_calls(&s, "a:2", true).unwrap().len(), 1);
}

/// The vector rows for `seq` of `a:1`: one only when a model embedded the turn.
fn turn_vecs(db: &std::path::Path, seq: i64) -> i64 {
    common::count(
        db,
        &format!(
            "SELECT COUNT(*) FROM turns_vec JOIN capture_turns ON turn = capture_turns.rowid \
             WHERE session_id = 'a:1' AND seq = {seq}"
        ),
    )
}

#[test]
fn a_turn_with_a_secret_is_stored_refused_and_not_embedded() {
    let (d, s) = store();
    assert!(capture::start_session(&s, &row(), "/tmp/proj").unwrap());
    assert!(capture::record_turn(&s, "a:1", "user", "list the files", 10).unwrap());
    let secret = "export API_KEY=abcd1234efgh5678";
    assert!(capture::record_turn(&s, "a:1", "assistant", secret, 11).unwrap());
    let turns = capture::turns(&s, "a:1").unwrap();
    let got: Vec<(i64, &str, &str)> = turns
        .iter()
        .map(|t| (t.seq, t.role.as_str(), t.text.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            (1, "user", "list the files"),
            (2, "assistant", "[refused: credential]")
        ]
    );
    // The hook's marker for a credential past the 4 KB cut, stored as it came.
    assert!(capture::record_turn(&s, "a:1", "user", "[refused: credential]", 12).unwrap());
    let db = d.path().join("state.db");
    // Storing never embeds (that would hold the store lock); the backfill does, outside it.
    assert_eq!(turn_vecs(&db, 1), 0, "record_turn never embeds");
    lightbulb::memory::embed_missing(&s, 200).unwrap();
    assert_eq!(turn_vecs(&db, 2), 0, "a refused turn is never embedded");
    assert_eq!(turn_vecs(&db, 3), 0, "nor a marker the hook sent");
    let embedded = i64::from(lightbulb::embed_model::installed());
    assert_eq!(turn_vecs(&db, 1), embedded);
    // A long turn is cut to 4 KB at a character boundary.
    let long = "é".repeat(capture::TURN_MAX);
    assert!(capture::record_turn(&s, "a:1", "user", &long, 12).unwrap());
    let last = capture::turns(&s, "a:1").unwrap().pop().unwrap();
    assert!(last.text.len() <= capture::TURN_MAX && last.text.len() > capture::TURN_MAX - 2);
    // No session row (never started, or in a hidden folder): nothing is kept.
    assert!(!capture::record_turn(&s, "a:2", "user", "hello", 13).unwrap());
    assert!(capture::turns(&s, "a:2").unwrap().is_empty());
}

#[test]
fn turns_off_records_nothing() {
    let e = Env::new();
    e.daemon(false, "capture_turns = false\n", &[]);
    let cwd = e.folder("proj");
    let prompt = |text: &str| {
        let p = e.payload("UserPromptSubmit", &cwd, json!({ "prompt": text }));
        assert!(e.claude_hook("user-prompt-submit", p).status.success());
    };
    e.claude_hook("session-start", e.payload("SessionStart", &cwd, json!({})));
    e.captured("the session starts", |_| true);
    prompt("not kept");
    let stop = json!({ "transcript_path": transcript() });
    e.claude_hook("stop", e.payload("Stop", &cwd, stop));
    // Capture does not wait for the write: give a wrong one time to land.
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(capture::turns(&e.store(), "claude:c1").unwrap().is_empty());
    // On again (the running daemon reads the file each time): the next prompt is kept.
    lightbulb::config::Config::set(e.state.path(), "capture_turns", "true").unwrap();
    prompt("kept");
    common::until(10, "the turn is written", || {
        !capture::turns(&e.store(), "claude:c1").unwrap().is_empty()
    });
    let turns = capture::turns(&e.store(), "claude:c1").unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].text, "kept");
}

#[test]
fn turns_older_than_turns_days_are_pruned() {
    let (d, s) = store();
    capture::start_session(&s, &row(), "/tmp/proj").unwrap();
    let day = 86_400;
    let now = 1_000 * day;
    capture::record_turn(&s, "a:1", "user", "an old question", now - 91 * day).unwrap();
    capture::record_turn(&s, "a:1", "user", "a new question", now - day).unwrap();
    assert_eq!(capture::prune_turns(&s, now, 90).unwrap(), 1);
    let left = capture::turns(&s, "a:1").unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].text, "a new question");
    // The index and the vectors follow the rows.
    let db = d.path().join("state.db");
    let fts = "SELECT COUNT(*) FROM turns_fts WHERE turns_fts MATCH 'old'";
    assert_eq!(common::count(&db, fts), 0);
    let orphans =
        "SELECT COUNT(*) FROM turns_vec WHERE turn NOT IN (SELECT rowid FROM capture_turns)";
    assert_eq!(common::count(&db, orphans), 0);
    drop(s);

    // The daemon prunes at start, by `capture_turns_days`.
    let e = Env::new();
    let db = e.store();
    capture::start_session(&db, &row(), "/tmp/proj").unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    capture::record_turn(&db, "a:1", "user", "a month ago", now - 31 * day).unwrap();
    capture::record_turn(&db, "a:1", "user", "today", now).unwrap();
    drop(db);
    e.daemon(false, "capture_turns_days = 30\n", &[]);
    common::until(10, "the old turn is pruned", || {
        capture::turns(&e.store(), "a:1").unwrap().len() == 1
    });
    assert_eq!(capture::turns(&e.store(), "a:1").unwrap()[0].text, "today");
}

#[test]
fn sessions_show_prints_turns_in_order() {
    let e = Env::new();
    e.daemon(false, "", &[]);
    let cwd = e.folder("proj");
    e.claude_hook("session-start", e.payload("SessionStart", &cwd, json!({})));
    e.captured("the session starts", |_| true);
    let steps = [
        (
            "user-prompt-submit",
            "UserPromptSubmit",
            json!({"prompt": "first question"}),
        ),
        ("stop", "Stop", json!({"transcript_path": transcript()})),
        (
            "user-prompt-submit",
            "UserPromptSubmit",
            json!({"prompt": "second\u{7}\nquestion"}),
        ),
    ];
    for (n, (arg, event, extra)) in steps.into_iter().enumerate() {
        e.claude_hook(arg, e.payload(event, &cwd, extra));
        common::until(10, "the turn is written", || {
            capture::turns(&e.store(), "claude:c1").unwrap().len() > n
        });
    }
    let show = out(&e.sessions(&["show", "claude:c1"]));
    let lines: Vec<&str> = show
        .lines()
        .filter(|l| l.contains("user: ") || l.contains("assistant: "))
        .collect();
    assert_eq!(
        lines,
        [
            "1 user: first question",
            "2 assistant: Done.",
            "3 user: second question"
        ],
        "{show}"
    );
}

#[test]
fn a_reply_equal_to_the_last_turn_is_recorded_once() {
    let (_d, s) = store();
    capture::start_session(&s, &row(), "/tmp/proj").unwrap();
    assert!(capture::record_turn(&s, "a:1", "user", "a question", 1).unwrap());
    assert!(capture::record_turn(&s, "a:1", "assistant", "an answer", 2).unwrap());
    // The transcript's last reply read again on the next Stop (the agent ended on a tool call).
    assert!(!capture::record_turn(&s, "a:1", "assistant", "an answer", 3).unwrap());
    // The same text by the other role is a new turn.
    assert!(capture::record_turn(&s, "a:1", "user", "an answer", 4).unwrap());
    let seqs: Vec<i64> = capture::turns(&s, "a:1")
        .unwrap()
        .iter()
        .map(|t| t.seq)
        .collect();
    assert_eq!(seqs, vec![1, 2, 3]);
}

/// The Stop's tokens and its reply travel in one `capture.stop`, so an answer that comes too
/// late for the hook costs neither: `LIGHTBULB_CONNECT_DEBUG_CAPTURE_DELAY_MS` slows every
/// capture op, here past the hook's wait, and the daemon still records both.
#[test]
fn a_slow_capture_stop_still_records_its_tokens_and_turn() {
    let e = Env::new();
    let delay = [(
        "LIGHTBULB_CONNECT_DEBUG_CAPTURE_DELAY_MS",
        "500".to_string(),
    )];
    e.daemon(false, "", &delay);
    let cwd = e.folder("proj");
    e.claude_hook("session-start", e.payload("SessionStart", &cwd, json!({})));
    e.captured("the session starts", |_| true);
    let stop = json!({"usage": {"input_tokens": 7, "output_tokens": 3},
        "last_assistant_message": "Done."});
    let o = e.claude_hook("stop", e.payload("Stop", &cwd, stop));
    assert!(o.status.success(), "{}", err(&o));
    let s = e.captured("the tokens", |s| s.prompt_tokens.is_some());
    assert_eq!((s.prompt_tokens, s.completion_tokens), (Some(7), Some(3)));
    let turns = capture::turns(&e.store(), "claude:c1").unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(
        (turns[0].role.as_str(), turns[0].text.as_str()),
        ("assistant", "Done.")
    );
}

/// The hook checks the whole prompt for a credential before the 4 KB cut.
#[test]
fn a_credential_past_the_cut_is_still_refused() {
    let e = Env::new();
    e.daemon(false, "", &[]);
    let cwd = e.folder("proj");
    e.claude_hook("session-start", e.payload("SessionStart", &cwd, json!({})));
    e.captured("the session starts", |_| true);
    let prompt = format!(
        "{} export API_KEY=abcd1234efgh5678",
        "x".repeat(capture::TURN_MAX)
    );
    let p = e.payload("UserPromptSubmit", &cwd, json!({ "prompt": prompt }));
    assert!(e.claude_hook("user-prompt-submit", p).status.success());
    common::until(10, "the turn is written", || {
        !capture::turns(&e.store(), "claude:c1").unwrap().is_empty()
    });
    let turns = capture::turns(&e.store(), "claude:c1").unwrap();
    assert_eq!(turns[0].text, "[refused: credential]");
}

/// A running session's first tool call opens its row and is recorded in one request: an answer
/// that comes too late for the hook (`LIGHTBULB_CONNECT_DEBUG_CAPTURE_DELAY_MS` slows every
/// capture op) never loses it.
#[test]
fn a_running_sessions_first_tool_call_survives_a_slow_answer() {
    let e = Env::new();
    let delay = [(
        "LIGHTBULB_CONNECT_DEBUG_CAPTURE_DELAY_MS",
        "500".to_string(),
    )];
    e.daemon(false, "", &delay);
    let cwd = e.folder("proj");
    let post = json!({"tool_name": "Read", "tool_input": {"file_path": "/x/y.rs"}});
    let o = e.claude_hook("post-tool-use", e.payload("PostToolUse", &cwd, post));
    assert!(o.status.success(), "{}", err(&o));
    let mut calls = vec![];
    common::until(10, "the call is recorded", || {
        calls = capture::tool_calls(&e.store(), "claude:c1", true).unwrap();
        !calls.is_empty()
    });
    assert_eq!(capture::sessions_since(&e.store(), -1).unwrap().len(), 1);
    assert_eq!(calls[0].summary.as_deref(), Some("Read: /x/y.rs"));
}
