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
        let me = os::process(std::process::id()).unwrap();
        let mut child = self
            .base_cmd(Command::new(EXE))
            .args(["hook", "claude", arg])
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
            (
                "stop",
                "Stop",
                json!({"usage": {"input_tokens": 120, "output_tokens": 45}}),
            ),
            ("session-end", "SessionEnd", json!({})),
        ];
        for (arg, event, extra) in steps {
            let o = self.claude_hook(arg, self.payload(event, cwd, extra));
            assert!(o.status.success(), "{}", err(&o));
        }
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
    let db = e.store();
    let rows = capture::sessions_since(&db, -1).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let s = &rows[0];
    assert_eq!((s.id.as_str(), s.harness.as_str()), ("claude:c1", "claude"));
    assert!(s.ended.is_some());
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
    let rows = capture::sessions_since(&e.store(), -1).unwrap();
    assert_eq!(
        (rows[0].prompt_tokens, rows[0].completion_tokens),
        (None, None)
    );
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
    assert!(!capture::start_session(&s, &row(), "/tmp/.secret/proj").unwrap());
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
    assert!(capture::sessions_since(&e.store(), -1).unwrap().is_empty());
}

#[test]
fn test_capture_write_failure_is_silent() {
    // The daemon cannot be reached (its socket path is too long to bind): exit 0, nothing on
    // stdout.
    let e = Env::new();
    let long = e.home.path().join("a".repeat(120));
    std::fs::create_dir_all(&long).unwrap();
    let me = os::process(std::process::id()).unwrap();
    for (arg, event) in [
        ("post-tool-use", "PostToolUse"),
        ("stop", "Stop"),
        ("session-end", "SessionEnd"),
    ] {
        let cwd = e.folder("proj");
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
        let p = e.payload(
            event,
            &cwd,
            json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}),
        );
        child
            .stdin
            .take()
            .unwrap()
            .write_all(p.to_string().as_bytes())
            .unwrap();
        let o = child.wait_with_output().unwrap();
        assert!(o.status.success(), "{arg}");
        assert!(o.stdout.is_empty(), "{arg}");
    }
    // A store that refuses (a session the daemon never saw) is not an error either.
    let (_d, s) = store();
    assert!(!capture::record_tool_call(&s, "nobody", "Bash", "{}", None, true).unwrap());
    // Nor is garbage on stdin, with a live daemon.
    e.daemon(false, "", &[]);
    let o = e.claude_hook("post-tool-use", json!("not an object"));
    assert!(o.status.success() && o.stdout.is_empty());
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

    let list = out(&e.sessions(&["list"]));
    let header: Vec<&str> = list.lines().next().unwrap().split_whitespace().collect();
    assert_eq!(header, ["Id", "Agent", "Repo", "Started", "In", "Out"]);
    let row = list.lines().nth(1).unwrap();
    assert!(
        row.contains("claude:c1") && row.contains("claude  app "),
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
