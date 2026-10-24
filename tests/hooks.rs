//! `hook <agent> <event>` run as the agents run it: the built binary, a fixture on stdin.
//! The caller is pinned with LIGHTBULB_CONNECT_HOOK_CALLER, since the test runner is not
//! an agent. That override is compiled out of release builds, so `cargo test --release`
//! fails the pinned tests here; run them in a debug build.

use lightbulb::{os, paths};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const EXE: &str = env!("CARGO_BIN_EXE_lightbulb");

struct Net {
    dir: tempfile::TempDir,
    claude: tempfile::TempDir,
    daemon: Child,
}

impl Net {
    fn new() -> Net {
        let dir = tempfile::tempdir().unwrap();
        let claude = tempfile::tempdir().unwrap();
        let daemon = Command::new(EXE)
            .arg("daemon")
            .env("LIGHTBULB_CONNECT_DEBUG_NO_DEFAULT_RELAYS", "1")
            .env(paths::HOME_ENV, dir.path())
            .env("LIGHTBULB_CONNECT_CLAUDE_HOME", claude.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let net = Net {
            dir,
            claude,
            daemon,
        };
        for _ in 0..50 {
            if UnixStream::connect(net.dir.path().join(paths::SOCKET)).is_ok() {
                return net;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("daemon socket never came up");
    }

    fn call(&self, req: Value) -> Value {
        let mut s = UnixStream::connect(self.dir.path().join(paths::SOCKET)).unwrap();
        s.write_all(format!("{req}\n").as_bytes()).unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn key(&self, provider: &str, conversation: &str) -> String {
        let short = self.call(json!({"id": 1, "op": "status"}))["member_key_short"]
            .as_str()
            .unwrap()
            .to_string();
        format!("{short}/{provider}:{conversation}")
    }

    fn send(&self, to: &str, text: &str, rid: &str) {
        let r = self.call(json!({"id": 1, "op": "send", "from": "x/codex:a", "to": to,
            "text": text, "request_id": rid}));
        assert_eq!(r["receipt"]["state"], "stored", "{r}");
    }

    fn receipt(&self, rid: &str) -> Value {
        self.call(json!({"id": 1, "op": "receipt", "from": "x/codex:a", "request_id": rid}))
            ["receipt"]["state"]
            .clone()
    }

    fn session(&self, key: &str) -> Option<Value> {
        self.call(json!({"id": 1, "op": "sessions"}))["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["key"] == key)
            .cloned()
    }

    /// Runs the hook with `tests/fixtures/hooks/<fixture>.json` on stdin, called as the
    /// agent; returns stdout.
    fn hook(&self, agent: &str, event: &str, fixture: &str) -> String {
        let me = os::process(std::process::id()).unwrap();
        self.hook_as(agent, event, fixture, Some(&me))
    }

    /// `pin: None` leaves the real caller (this test runner, which is no agent).
    fn hook_as(
        &self,
        agent: &str,
        event: &str,
        fixture: &str,
        pin: Option<&os::Process>,
    ) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/hooks")
            .join(format!("{fixture}.json"));
        let out = self.hook_raw(agent, event, &std::fs::read(path).unwrap(), pin);
        assert!(
            out.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// The hook with `stdin` as its payload; it must exit 0, whatever it was given.
    fn hook_raw(
        &self,
        agent: &str,
        event: &str,
        stdin: &[u8],
        pin: Option<&os::Process>,
    ) -> std::process::Output {
        let mut cmd = Command::new(EXE);
        if let Some(p) = pin {
            cmd.env(
                "LIGHTBULB_CONNECT_HOOK_CALLER",
                format!("{}:{}", p.pid, p.birth_micros),
            );
        }
        let mut child = cmd
            .args(["hook", agent, event])
            .env(paths::HOME_ENV, self.dir.path())
            .env("LIGHTBULB_CONNECT_CLAUDE_HOME", self.claude.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "a hook must always exit 0");
        out
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        let _ = UnixStream::connect(self.dir.path().join(paths::SOCKET))
            .and_then(|mut s| s.write_all(b"{\"id\":0,\"op\":\"shutdown\"}\n"));
        let _ = self.daemon.wait();
    }
}

fn frame_of(text: &str) -> String {
    format!("[Lightbulb teammate contribution from Your Codex {{\"requestId\":\"{text}\"}}]\nReply with send_message(reply_to=\"{text}\") to answer this message.\n\nbody {text}")
}

#[test]
fn stop_hook_hands_waiting_mail_to_grok_as_block_decision() {
    let net = Net::new();
    assert_eq!(net.hook("grok", "session-start", "grok-session-start"), "");
    let key = net.key("grok", "019a0000-0000-7000-8000-0000000000f1");
    assert!(net.session(&key).is_some());
    net.send(&key, "body r1", "r1");

    // A stop that is not the end of a turn is ignored and leaves the mail.
    assert_eq!(net.hook("grok", "stop", "grok-stop-shutdown"), "");
    assert_eq!(net.receipt("r1"), "stored");

    let out: Value = serde_json::from_str(&net.hook("grok", "stop", "grok-stop")).unwrap();
    assert_eq!(out, json!({"decision": "block", "reason": frame_of("r1")}));
    assert_eq!(net.receipt("r1"), "delivered");
    assert_eq!(net.session(&key).unwrap()["status"], "idle");
}

/// An agent that changes its hook format: whatever arrives on stdin that no agent's shape
/// names a conversation in (not JSON, another type, renamed fields, an event name we do not
/// know), the hook, run as that very agent, exits 0, prints nothing for the agent, registers
/// and ends no session, and leaves the waiting message waiting.
#[test]
fn an_unknown_hook_payload_exits_0_and_changes_nothing() {
    let net = Net::new();
    net.hook("claude", "session-start", "claude-session-start");
    let conv = "00000000-0000-4000-8000-0000000000c1";
    let key = net.key("claude", conv);
    net.send(&key, "body r1", "r1");
    let me = os::process(std::process::id()).unwrap();
    let unknown_event = json!({"hook_event_name": "SomethingNew", "session_id": conv});
    let renamed = json!({"sessionUuid": conv, "event": "Stop", "schema": 2});
    let wrong_types =
        json!({"session_id": 5, "conversation_id": [conv], "sessionId": {"id": conv}});
    let payloads = [
        String::new(),
        "not json".into(),
        "null".into(),
        "[1,2]".into(),
        "{}".into(),
        renamed.to_string(),
        wrong_types.to_string(),
    ];
    for agent in ["claude", "codex", "cursor", "grok"] {
        // Cursor also reads `session_id`: an unknown event name is only unknown to the two
        // agents that name the event in the payload.
        let extra = ["claude", "codex"]
            .contains(&agent)
            .then(|| unknown_event.to_string());
        for event in lightbulb::hook::EVENTS {
            for stdin in payloads.iter().chain(extra.as_ref()) {
                let out = net.hook_raw(agent, event.arg(), stdin.as_bytes(), Some(&me));
                assert!(out.stdout.is_empty(), "{agent} {} {stdin}", event.arg());
            }
        }
    }
    let sessions = net.call(json!({"id": 1, "op": "sessions"}))["sessions"].clone();
    assert_eq!(sessions.as_array().map(Vec::len), Some(1), "{sessions}");
    assert!(net.session(&key).is_some());
    assert_eq!(net.receipt("r1"), "stored");
}

#[test]
fn the_receipt_walks_stored_delivered_read_through_the_hooks() {
    let net = Net::new();
    net.hook("grok", "session-start", "grok-session-start");
    let key = net.key("grok", "019a0000-0000-7000-8000-0000000000f1");
    net.send(&key, "body r1", "r1");
    assert_eq!(net.receipt("r1"), "stored");
    assert_ne!(net.hook("grok", "stop", "grok-stop"), "");
    assert_eq!(net.receipt("r1"), "delivered");
    // The agent's next hook event, a later second: it has had the message in front of it.
    std::thread::sleep(Duration::from_millis(1100));
    net.hook("grok", "post-tool-use", "grok-post-tool-use");
    assert_eq!(net.receipt("r1"), "read");
}

#[test]
fn session_end_removes_the_record_and_keeps_its_mail() {
    let net = Net::new();
    net.hook("grok", "session-start", "grok-session-start");
    let key = net.key("grok", "019a0000-0000-7000-8000-0000000000f1");
    assert!(net.session(&key).is_some());
    net.send(&key, "body r1", "r1");

    assert_eq!(net.hook("grok", "session-end", "grok-session-end"), "");
    assert!(net.session(&key).is_none());
    assert_eq!(net.receipt("r1"), "stored", "mail waits out the hold");
    // A resume registers the same conversation again and its next stop gets the mail.
    assert_eq!(net.hook("grok", "session-start", "grok-session-start"), "");
    let out: Value = serde_json::from_str(&net.hook("grok", "stop", "grok-stop")).unwrap();
    assert_eq!(out, json!({"decision": "block", "reason": frame_of("r1")}));
    assert_eq!(net.receipt("r1"), "delivered");
}

fn register(net: &Net, conversation: &str, pid: u32, birth: u64) -> String {
    net.call(
        json!({"id": 1, "op": "register", "session": {"provider": "claude",
        "conversation": conversation, "pid": pid, "birth_micros": birth, "cwd": "/"}}),
    )["key"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn a_session_end_after_the_agent_exited_still_ends_the_session() {
    let net = Net::new();
    let key = register(&net, "00000000-0000-4000-8000-0000000000c1", 999_999, 1);
    // The caller check fails (no agent above the hook), yet the recorded process is gone.
    assert_eq!(
        net.hook_as("claude", "session-end", "claude-session-end", None),
        ""
    );
    let r = net.call(json!({"id": 1, "op": "end", "key": key}));
    assert_eq!(r["error"], "not_found", "the row was already ended");
}

#[test]
fn a_hand_run_session_end_cannot_end_a_live_session() {
    let net = Net::new();
    let me = os::process(std::process::id()).unwrap();
    let key = register(
        &net,
        "00000000-0000-4000-8000-0000000000c1",
        me.pid,
        me.birth_micros,
    );
    assert_eq!(
        net.hook_as("claude", "session-end", "claude-session-end", None),
        ""
    );
    assert!(net.session(&key).is_some());
    // The agent itself ending it works.
    net.hook("claude", "session-end", "claude-session-end");
    assert!(net.session(&key).is_none());
}

#[test]
fn claude_post_tool_use_prints_additional_context_with_the_frame() {
    let net = Net::new();
    assert_eq!(
        net.hook("claude", "session-start", "claude-session-start"),
        ""
    );
    let key = net.key("claude", "00000000-0000-4000-8000-0000000000c1");
    net.send(&key, "body r1", "r1");
    let out: Value =
        serde_json::from_str(&net.hook("claude", "post-tool-use", "claude-post-tool-use")).unwrap();
    assert_eq!(
        out,
        json!({"hookSpecificOutput": {"hookEventName": "PostToolUse",
            "additionalContext": frame_of("r1")}})
    );
    assert_eq!(net.session(&key).unwrap()["status"], "live");
    // Drained: the next tool event prints nothing.
    assert_eq!(
        net.hook("claude", "post-tool-use", "claude-post-tool-use"),
        ""
    );
}

#[test]
fn cursor_stop_prints_followup_message() {
    let net = Net::new();
    assert_eq!(
        net.hook("cursor", "session-start", "cursor-session-start"),
        ""
    );
    let key = net.key("cursor", "5c1e0000-0000-4000-8000-0000000000e1");
    net.send(&key, "body r1", "r1");
    net.send(&key, "body r2", "r2");
    let out: Value = serde_json::from_str(&net.hook("cursor", "stop", "cursor-stop")).unwrap();
    assert_eq!(
        out,
        json!({"followup_message": format!("{}\n\n{}", frame_of("r1"), frame_of("r2"))})
    );
}

#[test]
fn register_refuses_a_bad_conversation_id() {
    let net = Net::new();
    let reg = |provider: &str, conversation: &str, status: &str| {
        net.call(
            json!({"id": 1, "op": "register", "session": {"provider": provider,
            "conversation": conversation, "pid": 1, "birth_micros": 1, "cwd": "/",
            "status": status}}),
        )
    };
    assert_eq!(reg("grok", "a b", "idle")["error"], "bad_request");
    assert_eq!(reg("grok", "", "idle")["error"], "bad_request");
    assert_eq!(
        reg("grok", &"a".repeat(129), "idle")["error"],
        "bad_request"
    );
    assert_eq!(reg("vim", "ok", "idle")["error"], "bad_request");
    assert_eq!(reg("grok", "ok", "asleep")["error"], "bad_request");
    assert_eq!(reg("grok", "o.k_-1", "busy")["ok"], true);
}

/// Another live process for a session to belong to; killed on drop.
struct Other(Child, os::Process);

impl Other {
    fn new() -> Other {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let p = os::process(child.id()).unwrap();
        Other(child, p)
    }
}

impl Drop for Other {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const C1: &str = "00000000-0000-4000-8000-0000000000c1";

#[test]
fn session_end_pinned_to_another_live_process_does_not_end_the_row() {
    let net = Net::new();
    let other = Other::new();
    net.hook_as(
        "claude",
        "session-start",
        "claude-session-start",
        Some(&other.1),
    );
    let key = net.key("claude", C1);
    // A hook from this test's own (live, different) process.
    assert_eq!(net.hook("claude", "session-end", "claude-session-end"), "");
    assert!(net.session(&key).is_some());
    net.hook_as(
        "claude",
        "session-end",
        "claude-session-end",
        Some(&other.1),
    );
    assert!(net.session(&key).is_none());
}

#[test]
fn register_cannot_overwrite_a_key_owned_by_another_live_process() {
    let net = Net::new();
    let other = Other::new();
    net.hook_as(
        "claude",
        "session-start",
        "claude-session-start",
        Some(&other.1),
    );
    let me = os::process(std::process::id()).unwrap();
    let r = net.call(
        json!({"id": 1, "op": "register", "session": {"provider": "claude",
        "conversation": C1, "pid": me.pid, "birth_micros": me.birth_micros, "cwd": "/"},
        "caller": {"pid": me.pid, "birth_micros": me.birth_micros}}),
    );
    assert_eq!(
        (r["error"].clone(), r["reason"].clone()),
        (json!("bad_request"), json!("owned_by_live_process"))
    );
    let key = net.key("claude", C1);
    assert_eq!(net.session(&key).unwrap()["pid"], other.1.pid);
    // The owner itself may re-register.
    net.hook_as(
        "claude",
        "user-prompt-submit",
        "claude-user-prompt-submit",
        Some(&other.1),
    );
    assert_eq!(net.session(&key).unwrap()["status"], "live");
}

#[test]
fn inbox_from_another_process_is_refused() {
    let net = Net::new();
    let other = Other::new();
    net.hook_as(
        "grok",
        "session-start",
        "grok-session-start",
        Some(&other.1),
    );
    let key = net.key("grok", "019a0000-0000-7000-8000-0000000000f1");
    net.send(&key, "body r1", "r1");
    let me = os::process(std::process::id()).unwrap();
    let r = net.call(json!({"id": 1, "op": "inbox", "key": key,
        "caller": {"pid": me.pid, "birth_micros": me.birth_micros}}));
    assert_eq!(
        (r["error"].clone(), r["reason"].clone()),
        (json!("bad_request"), json!("not_owner"))
    );
    let r = net.call(json!({"id": 1, "op": "inbox", "key": key,
        "caller": {"pid": other.1.pid, "birth_micros": other.1.birth_micros}}));
    assert_eq!(r["messages"][0]["text"], "body r1");
}

#[test]
fn a_discovered_claude_session_drains_its_own_inbox() {
    let net = Net::new();
    let me = os::process(std::process::id()).unwrap();
    let dir = net.claude.path().join(".claude/sessions");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{}.json", me.pid)),
        json!({"pid": me.pid, "sessionId": "found-only", "cwd": "/tmp",
            "startedAt": me.birth_micros / 1000})
        .to_string(),
    )
    .unwrap();
    let key = net.key("claude", "found-only");
    net.send(&key, "hello", "r1");

    // Never registered: the owner is the discovered session's process.
    let r = net.call(json!({"id": 1, "op": "inbox", "key": key,
        "caller": {"pid": me.pid, "birth_micros": me.birth_micros}}));
    assert_eq!(r["messages"][0]["text"], "hello", "{r}");
    assert_eq!(net.receipt("r1"), "delivered");
    // Someone else is still refused.
    let r = net.call(json!({"id": 1, "op": "inbox", "key": key,
        "caller": {"pid": me.pid, "birth_micros": 1}}));
    assert_eq!(r["reason"], "not_owner", "{r}");
}

/// Runs `hook claude session-start` pinned to `caller`; returns its stderr.
fn claude_start_as(net: &Net, caller: &os::Process) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hooks/claude-session-start.json");
    let out = Command::new(EXE)
        .args(["hook", "claude", "session-start"])
        .env(
            "LIGHTBULB_CONNECT_HOOK_CALLER",
            format!("{}:{}", caller.pid, caller.birth_micros),
        )
        .env(paths::HOME_ENV, net.dir.path())
        .env("LIGHTBULB_CONNECT_CLAUDE_HOME", net.claude.path())
        .stdin(std::fs::File::open(path).unwrap())
        .output()
        .unwrap();
    assert!(out.status.success(), "a hook must always exit 0");
    assert!(out.stdout.is_empty());
    String::from_utf8(out.stderr).unwrap()
}

#[test]
fn a_claude_hook_fired_by_cursor_registers_nothing() {
    let net = Net::new();
    let key = net.key("claude", C1);
    // A process at Cursor's app path (a copy of `sleep`) ran the Claude hook: nothing.
    let app = tempfile::tempdir().unwrap();
    let exe = app.path().join("Cursor.app/Contents/MacOS/Cursor");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::copy("/bin/sleep", &exe).unwrap();
    let mut cursor = Command::new(&exe).arg("30").spawn().unwrap();
    let p = os::process(cursor.id()).unwrap();
    let err = claude_start_as(&net, &p);
    assert!(err.contains("fired by Cursor"), "{err}");
    assert!(net.session(&key).is_none());
    let _ = cursor.kill();
    let _ = cursor.wait();
    // The same hook from a plain process registers the Claude session.
    let other = Other::new();
    assert_eq!(claude_start_as(&net, &other.1), "");
    assert!(net.session(&key).is_some());
}
