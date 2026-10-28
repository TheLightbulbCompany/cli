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
        Net::with_env(&[])
    }

    /// A daemon with `env` set too.
    fn with_env(env: &[(&str, &str)]) -> Net {
        let dir = tempfile::tempdir().unwrap();
        let claude = tempfile::tempdir().unwrap();
        let daemon = Command::new(EXE)
            .arg("daemon")
            .envs(env.iter().copied())
            .env("LIGHTBULB_CONNECT_DEBUG_NO_DEFAULT_RELAYS", "1")
            .env("LIGHTBULB_CONNECT_DEBUG_NO_TELEMETRY", "1")
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
        // Nothing on stderr but, on a loaded machine, capture's one "not waiting" line.
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr
                .lines()
                .all(|l| l.contains(": capture took over 10 ms; not waiting for it")),
            "{stderr}"
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
        cmd.env("LIGHTBULB_CONNECT_DEBUG_NO_TELEMETRY", "1");
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

/// OpenCode's hook offers mail without taking it; only the plugin's `ack`, sent once the
/// mail is in the session, delivers it. Unacked mail is offered again; an ack moves only the
/// acking session's own mail.
#[test]
fn opencode_peek_then_ack() {
    let net = Net::new();
    let me = os::process(std::process::id()).unwrap();
    let conv = "ses_0a1b2c3d4e5fOpEnCoDe0001";
    // session-start registers and offers no mail.
    assert_eq!(
        net.hook("opencode", "session-start", "opencode-session-start"),
        ""
    );
    let key = net.key("opencode", conv);
    assert!(net.session(&key).is_some());
    net.send(&key, "body r1", "r1");
    assert_eq!(
        net.hook("opencode", "session-start", "opencode-session-start"),
        ""
    );

    // Offered, and still waiting.
    let peek = || -> Value {
        serde_json::from_str(&net.hook("opencode", "post-tool-use", "opencode-post-tool-use"))
            .unwrap()
    };
    let out = peek();
    assert_eq!(out["text"], frame_of("r1"));
    let ids = out["ids"].clone();
    assert_eq!(ids.as_array().map(Vec::len), Some(1), "{out}");
    assert_eq!(net.receipt("r1"), "stored");
    assert_eq!(net.session(&key).unwrap()["status"], "live");
    // Not acked (the push failed or the plugin died): offered again, the same.
    assert_eq!(peek(), out);
    assert_eq!(net.receipt("r1"), "stored");

    let ack = |conv: &str, pin: &os::Process| {
        let p = json!({"session_id": conv, "cwd": "/tmp/proj", "ids": ids}).to_string();
        let out = net.hook_raw("opencode", "ack", p.as_bytes(), Some(pin));
        assert!(out.stdout.is_empty());
    };
    // Another session acking this one's ids moves nothing.
    let other = "ses_0a1b2c3d4e5fOpEnCoDe0009";
    let p = json!({"session_id": other, "cwd": "/tmp/proj"}).to_string();
    net.hook_raw("opencode", "session-start", p.as_bytes(), Some(&me));
    assert!(net.session(&net.key("opencode", other)).is_some());
    ack(other, &me);
    assert_eq!(net.receipt("r1"), "stored");
    // Nor does an ack for this session from a process that is not its own.
    let stranger = os::Process {
        pid: 1,
        birth_micros: 1,
        executable: PathBuf::new(),
    };
    ack(conv, &stranger);
    assert_eq!(net.receipt("r1"), "stored");

    // Its own ack delivers it, and it is not offered again.
    ack(conv, &me);
    assert_eq!(net.receipt("r1"), "delivered");
    assert_eq!(
        net.hook("opencode", "post-tool-use", "opencode-post-tool-use"),
        ""
    );
    // Acking twice is harmless.
    ack(conv, &me);

    net.send(&key, "body r2", "r2");
    let out: Value = serde_json::from_str(&net.hook("opencode", "stop", "opencode-stop")).unwrap();
    assert_eq!(out["text"], frame_of("r2"));
    assert_eq!(net.session(&key).unwrap()["status"], "idle");

    assert_eq!(
        net.hook("opencode", "session-end", "opencode-session-end"),
        ""
    );
    assert!(net.session(&key).is_none());
}

/// The OpenCode plugin asks for no mail when it could not hand it over with the session's
/// own agent: the hook still registers the session and the mail waits.
#[test]
fn an_opencode_hook_that_asks_for_no_mail_leaves_it_waiting() {
    let net = Net::new();
    let me = os::process(std::process::id()).unwrap();
    let conv = "ses_0a1b2c3d4e5fOpEnCoDe0002";
    let payload =
        |mail: bool| json!({"session_id": conv, "cwd": "/tmp/proj", "mail": mail}).to_string();
    let out = net.hook_raw(
        "opencode",
        "post-tool-use",
        payload(false).as_bytes(),
        Some(&me),
    );
    assert!(out.stdout.is_empty());
    let key = net.key("opencode", conv);
    assert!(net.session(&key).is_some());
    net.send(&key, "body r1", "r1");
    for arg in ["post-tool-use", "stop"] {
        let out = net.hook_raw("opencode", arg, payload(false).as_bytes(), Some(&me));
        assert!(out.stdout.is_empty(), "{arg}");
    }
    assert_eq!(net.receipt("r1"), "stored");
    let out = net.hook_raw("opencode", "stop", payload(true).as_bytes(), Some(&me));
    let out: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out["text"], frame_of("r1"));
    assert_eq!(net.receipt("r1"), "stored");
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
    for agent in ["claude", "codex", "cursor", "grok", "opencode"] {
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
            "startedAt": lightbulb::os::started_ms(me.pid).unwrap()})
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
        .env("LIGHTBULB_CONNECT_DEBUG_NO_TELEMETRY", "1")
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
    // Copied by a child `cp`, not by this process: on Linux a parallel test's fork inherits any
    // write fd we hold until it execs, and exec of a file open for writing fails (ETXTBSY).
    let cp = Command::new("/bin/cp").arg("/bin/sleep").arg(&exe).status();
    assert!(cp.unwrap().success());
    let mut cursor = Command::new(&exe).arg("30").spawn().unwrap();
    let p = os::process(cursor.id()).unwrap();
    let err = claude_start_as(&net, &p);
    assert!(err.contains("fired by Cursor"), "{err}");
    assert!(net.session(&key).is_none());
    let _ = cursor.kill();
    let _ = cursor.wait();
    // The same hook from a plain process registers the Claude session.
    let other = Other::new();
    // Nothing on stderr but, on a loaded machine, capture's one "not waiting" line.
    let err = claude_start_as(&net, &other.1);
    assert!(
        err.lines()
            .all(|l| l.contains(": capture took over 10 ms; not waiting for it")),
        "{err}"
    );
    assert!(net.session(&key).is_some());
}

/// A folder whose origin is `<name>.git`, so a session started in it has that repo.
fn repo_dir(name: &str) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join(".git")).unwrap();
    std::fs::write(d.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(
        d.path().join(".git/config"),
        format!("[remote \"origin\"]\n\turl = git@github.com:Org/{name}.git\n"),
    )
    .unwrap();
    d
}

/// Runs the Claude hook for `conversation` in `cwd`; returns stdout.
fn claude_hook(
    net: &Net,
    event: &str,
    name: &str,
    conversation: &str,
    cwd: &std::path::Path,
) -> String {
    let me = os::process(std::process::id()).unwrap();
    let payload = json!({"session_id": conversation, "cwd": cwd, "hook_event_name": name});
    let out = net.hook_raw("claude", event, payload.to_string().as_bytes(), Some(&me));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr
            .lines()
            .all(|l| l.contains(": capture took over 10 ms; not waiting for it")),
        "{stderr}"
    );
    String::from_utf8(out.stdout).unwrap()
}

fn context(out: &str) -> String {
    let v: Value = serde_json::from_str(out).unwrap();
    v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_string()
}

fn remember_in(net: &Net, session: &str, subject: &str, fact: &str, scope: &str) {
    let r = net.call(
        json!({"id": 1, "op": "memory.remember", "subject": subject, "fact": fact,
        "kind": "decision", "scope": scope, "session": session}),
    );
    assert_eq!(r["stored"], true, "{r}");
}

const A: &str = "00000000-0000-4000-8000-0000000000a1";
const B: &str = "00000000-0000-4000-8000-0000000000b2";
const INSTRUCTION: &str = "Call `recall` when a task needs something a teammate may know; call `remember` when you finish a task or are told to remember. Personal unless the person says to share.";

#[test]
fn session_start_includes_memory_block_for_repo() {
    let net = Net::new();
    let (cli, other) = (repo_dir("cli"), repo_dir("other"));
    assert_eq!(
        claude_hook(&net, "session-start", "SessionStart", A, cli.path()),
        ""
    );
    let a = net.key("claude", A);
    remember_in(&net, &a, "db", "postgres 16", "me");
    remember_in(&net, &a, "ci", "github actions", "team");
    let text = context(&claude_hook(
        &net,
        "session-start",
        "SessionStart",
        B,
        cli.path(),
    ));
    let (page, tail) = text.split_once("Memory (team and yours):\n").unwrap();
    assert_eq!(page, "");
    assert!(tail.ends_with(&format!("\n{INSTRUCTION}")), "{tail}");
    assert!(tail.find("ci: github actions").unwrap() < tail.find("db: postgres 16").unwrap());
    // The block is kept for `memory last`, and another repo gets none.
    let last = net.dir.path().join("state.db");
    let conn = rusqlite::Connection::open(last).unwrap();
    let block: String = conn
        .query_row(
            "SELECT block FROM memory_last WHERE session = ?1",
            [net.key("claude", B)],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        block,
        text.split_once("Memory (team and yours):")
            .map(|(_, t)| format!("Memory (team and yours):{t}"))
            .unwrap()
    );
    assert_eq!(
        claude_hook(
            &net,
            "session-start",
            "SessionStart",
            "other-1",
            other.path()
        ),
        ""
    );
}

#[test]
fn test_session_block_caps_at_1600_chars() {
    let net = Net::new();
    let cli = repo_dir("cli");
    claude_hook(&net, "session-start", "SessionStart", A, cli.path());
    let a = net.key("claude", A);
    for i in 0..60 {
        remember_in(&net, &a, &format!("topic {i:02}"), &"x".repeat(100), "team");
    }
    let text = context(&claude_hook(
        &net,
        "session-start",
        "SessionStart",
        B,
        cli.path(),
    ));
    let tail = text.split_once("Memory (team and yours):\n").unwrap().1;
    let page = tail.strip_suffix(&format!("\n{INSTRUCTION}")).unwrap();
    assert!(page.chars().count() <= 1600, "{}", page.chars().count());
    assert!(page.chars().count() > 1400);
    assert!(
        page.lines()
            .all(|l| l.starts_with("Current:") || l.starts_with("topic ")),
        "{page}"
    );
    assert!(
        page.lines().last().unwrap().ends_with(')'),
        "cut on a line boundary"
    );
}

#[test]
fn user_prompt_submit_never_includes_memory() {
    let net = Net::new();
    let cli = repo_dir("cli");
    claude_hook(&net, "session-start", "SessionStart", A, cli.path());
    remember_in(&net, &net.key("claude", A), "db", "postgres 16", "team");
    for (event, name) in [
        ("user-prompt-submit", "UserPromptSubmit"),
        ("post-tool-use", "PostToolUse"),
    ] {
        assert_eq!(claude_hook(&net, event, name, A, cli.path()), "");
    }
}

#[test]
fn a_failing_memory_recall_still_hands_over_the_mail() {
    let net = Net::new();
    let cli = repo_dir("cli");
    claude_hook(&net, "session-start", "SessionStart", A, cli.path());
    remember_in(&net, &net.key("claude", A), "db", "postgres 16", "team");
    // Break the memory store under the running daemon: recall answers an error.
    rusqlite::Connection::open(net.dir.path().join("state.db"))
        .unwrap()
        .execute_batch("DROP TRIGGER memory_ad; DROP TRIGGER memory_ai; DROP TABLE memory_entries;")
        .unwrap();
    assert_eq!(
        net.call(json!({"id": 1, "op": "memory.recall", "question": ""}))["ok"],
        false
    );
    net.send(&net.key("claude", B), "body r1", "r1");
    // claude_hook asserts stderr is empty.
    let text = context(&claude_hook(
        &net,
        "session-start",
        "SessionStart",
        B,
        cli.path(),
    ));
    assert_eq!(text, frame_of("r1"));
}

/// The prompt comes from UserPromptSubmit; the reply from Stop: the last assistant message of
/// a Claude Code transcript, and what a Codex Stop payload carries.
#[test]
fn a_prompt_and_a_reply_are_recorded_as_turns() {
    let net = Net::new();
    let cwd = tempfile::tempdir().unwrap();
    let me = os::process(std::process::id()).unwrap();
    let transcript = format!(
        "{}/tests/fixtures/hooks/claude-transcript.jsonl",
        env!("CARGO_MANIFEST_DIR")
    );
    let codex = "0199aaaa-0000-7000-8000-0000000000d1";
    let steps = [
        (
            "claude",
            "session-start",
            A,
            json!({"hook_event_name": "SessionStart"}),
        ),
        (
            "claude",
            "user-prompt-submit",
            A,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "list the files"}),
        ),
        (
            "claude",
            "stop",
            A,
            json!({"hook_event_name": "Stop", "transcript_path": transcript}),
        ),
        (
            "codex",
            "session-start",
            codex,
            json!({"hook_event_name": "SessionStart"}),
        ),
        (
            "codex",
            "user-prompt-submit",
            codex,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "fix the build"}),
        ),
        (
            "codex",
            "stop",
            codex,
            json!({"hook_event_name": "Stop", "last_assistant_message": "Fixed it."}),
        ),
    ];
    for (agent, event, conv, mut payload) in steps {
        payload["session_id"] = json!(conv);
        payload["cwd"] = json!(cwd.path());
        net.hook_raw(agent, event, payload.to_string().as_bytes(), Some(&me));
        // In order: the daemon writes each turn before the next hook runs.
        std::thread::sleep(Duration::from_millis(50));
    }
    let turns = |id: &str| -> Vec<(String, String)> {
        let db = lightbulb::store::Store::open(&net.dir.path().join("state.db")).unwrap();
        lightbulb::capture::turns(&db, id)
            .unwrap()
            .into_iter()
            .map(|t| (t.role, t.text))
            .collect()
    };
    let pair = |r: &str, t: &str| (r.to_string(), t.to_string());
    let want = [
        (
            format!("claude:{A}"),
            vec![pair("user", "list the files"), pair("assistant", "Done.")],
        ),
        (
            format!("codex:{codex}"),
            vec![
                pair("user", "fix the build"),
                pair("assistant", "Fixed it."),
            ],
        ),
    ];
    for (id, turns_want) in want {
        let end = std::time::Instant::now() + Duration::from_secs(10);
        while turns(&id).len() < 2 {
            assert!(std::time::Instant::now() < end, "{id}: {:?}", turns(&id));
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(turns(&id), turns_want, "{id}");
    }
}

/// A folder whose origin is `url`.
fn repo_at(url: &str) -> tempfile::TempDir {
    let d = repo_dir("x");
    std::fs::write(
        d.path().join(".git/config"),
        format!("[remote \"origin\"]\n\turl = {url}\n"),
    )
    .unwrap();
    d
}

#[test]
fn a_session_in_a_subfolder_gets_the_repo_block() {
    let net = Net::new();
    let cli = repo_dir("cli");
    let deep = cli.path().join("src/net");
    std::fs::create_dir_all(&deep).unwrap();
    claude_hook(&net, "session-start", "SessionStart", A, cli.path());
    remember_in(&net, &net.key("claude", A), "ci", "github actions", "team");
    let text = context(&claude_hook(
        &net,
        "session-start",
        "SessionStart",
        B,
        &deep,
    ));
    assert!(text.contains("ci: github actions"), "{text}");
    let repo = |c| net.session(&net.key("claude", c)).unwrap()["repo"].clone();
    assert_eq!(repo(B), repo(A));
}

#[test]
fn the_repo_key_carries_the_owner() {
    let net = Net::new();
    let ours = repo_at("git@github.com:acme/api.git");
    let theirs = repo_at("https://github.com/other/api");
    let ours_https = repo_at("https://github.com/acme/api.git");
    claude_hook(&net, "session-start", "SessionStart", A, ours.path());
    assert_eq!(
        net.session(&net.key("claude", A)).unwrap()["repo"],
        "acme/api"
    );
    remember_in(&net, &net.key("claude", A), "ci", "github actions", "team");
    // Another owner's `api` gets no block; the same repo cloned over https does.
    let other = claude_hook(&net, "session-start", "SessionStart", B, theirs.path());
    assert_eq!(other, "");
    assert_eq!(
        net.session(&net.key("claude", B)).unwrap()["repo"],
        "other/api"
    );
    let c = "00000000-0000-4000-8000-0000000000c3";
    let text = context(&claude_hook(
        &net,
        "session-start",
        "SessionStart",
        c,
        ours_https.path(),
    ));
    assert!(text.contains("ci: github actions"), "{text}");
}

/// Runs `agent`'s hook with `payload` on stdin, called as the agent; returns stdout.
fn agent_hook(net: &Net, agent: &str, event: &str, payload: Value) -> String {
    let me = os::process(std::process::id()).unwrap();
    let out = net.hook_raw(agent, event, payload.to_string().as_bytes(), Some(&me));
    String::from_utf8(out.stdout).unwrap()
}

/// Cursor adds a sessionStart hook's `additional_context` to the conversation's initial
/// context (cursor.com/docs/hooks), so a Cursor session starts with the repo's block. The start
/// carries the block only: waiting mail is not marked delivered there, and the stop hands it over.
#[test]
fn cursor_session_start_includes_the_memory_block() {
    let net = Net::new();
    let cli = repo_dir("cli");
    claude_hook(&net, "session-start", "SessionStart", A, cli.path());
    remember_in(&net, &net.key("claude", A), "ci", "github actions", "team");
    let conv = "5c1e0000-0000-4000-8000-0000000000e2";
    let cursor = |event: &str| {
        agent_hook(
            &net,
            "cursor",
            event,
            json!({"conversation_id": conv, "session_id": conv, "workspace_roots": [cli.path()],
                "hook_event_name": event}),
        )
    };
    let block = |out: &str| -> String {
        let out: Value = serde_json::from_str(out).unwrap();
        out["additional_context"].as_str().unwrap().to_string()
    };
    let first = block(&cursor("session-start"));
    net.send(&net.key("cursor", conv), "body r1", "r1");
    // Started again (resumed) with the mail waiting: the same block, and no mail.
    let text = block(&cursor("session-start"));
    assert_eq!(text, first);
    assert!(text.starts_with("Memory (team and yours):\n"), "{text}");
    assert!(text.contains("ci: github actions"), "{text}");
    assert!(text.ends_with(INSTRUCTION), "{text}");
    assert_eq!(net.receipt("r1"), "stored");
    let out: Value = serde_json::from_str(&cursor("stop")).unwrap();
    assert_eq!(out, json!({"followup_message": frame_of("r1")}));
    assert_eq!(net.receipt("r1"), "delivered");
}

/// Our OpenCode plugin asks the start hook for the block before the session's first request and
/// adds it to the system prompt: the start offers the block only, and the mail waits for a tool
/// call or a stop, which offer it as before.
#[test]
fn opencode_session_start_includes_the_memory_block() {
    let net = Net::new();
    let cli = repo_dir("cli");
    claude_hook(&net, "session-start", "SessionStart", A, cli.path());
    remember_in(&net, &net.key("claude", A), "ci", "github actions", "team");
    let conv = "ses_0a1b2c3d4e5fOpEnCoDe0003";
    let run = |event: &str, mail: bool| {
        agent_hook(
            &net,
            "opencode",
            event,
            json!({"session_id": conv, "cwd": cli.path(), "mail": mail}),
        )
    };
    // At session.created the plugin asks for no mail: it registers, and nothing is offered.
    assert_eq!(run("session-start", false), "");
    let key = net.key("opencode", conv);
    net.send(&key, "body r1", "r1");
    let out: Value = serde_json::from_str(&run("session-start", true)).unwrap();
    let text = out["text"].as_str().unwrap();
    let block = text.strip_prefix("Memory (team and yours):\n").unwrap();
    assert!(block.contains("ci: github actions"), "{block}");
    assert!(block.ends_with(INSTRUCTION), "{block}");
    assert!(!text.contains("body r1"), "{text}");
    assert_eq!(out["ids"], json!([]), "{out}");
    assert_eq!(net.receipt("r1"), "stored");
    let out: Value = serde_json::from_str(&run("post-tool-use", true)).unwrap();
    assert_eq!(out["text"], frame_of("r1"));
    assert_eq!(out["ids"].as_array().map(Vec::len), Some(1), "{out}");
}

/// Runs the Claude hook `event` for conversation `A` in `cwd` with `extra` in its payload;
/// returns stdout.
fn claude_step(net: &Net, event: &str, extra: Value, cwd: &std::path::Path) -> String {
    let me = os::process(std::process::id()).unwrap();
    let mut payload = extra;
    payload["session_id"] = json!(A);
    payload["cwd"] = json!(cwd);
    let out = net.hook_raw("claude", event, payload.to_string().as_bytes(), Some(&me));
    String::from_utf8(out.stdout).unwrap()
}

/// A tool call, then a wait: its hook waits 10 ms only, so the daemon has written it before the
/// next step.
fn tool_call(net: &Net, cwd: &std::path::Path) {
    let call = json!({"hook_event_name": "PostToolUse", "tool_name": "Bash",
        "tool_input": {"command": "ls"}});
    assert_eq!(claude_step(net, "post-tool-use", call, cwd), "");
    std::thread::sleep(Duration::from_millis(50));
}

/// A Stop: its hook waits for the nudge answer, so no wait after.
fn stop(net: &Net, cwd: &std::path::Path) -> String {
    claude_step(net, "stop", json!({"hook_event_name": "Stop"}), cwd)
}

fn started(net: &Net) -> tempfile::TempDir {
    let cwd = tempfile::tempdir().unwrap();
    let start = json!({"hook_event_name": "SessionStart"});
    assert_eq!(claude_step(net, "session-start", start, cwd.path()), "");
    std::thread::sleep(Duration::from_millis(50));
    cwd
}

#[test]
fn the_first_stop_after_a_tool_call_nudges_once() {
    let net = Net::new();
    let cwd = started(&net);
    tool_call(&net, cwd.path());
    let out: Value = serde_json::from_str(&stop(&net, cwd.path())).unwrap();
    assert_eq!(
        out,
        json!({"decision": "block", "reason": lightbulb::hook::NUDGE})
    );
    tool_call(&net, cwd.path());
    assert_eq!(stop(&net, cwd.path()), "");
}

#[test]
fn a_stop_with_waiting_messages_hands_them_over_and_does_not_nudge() {
    let net = Net::new();
    let cwd = started(&net);
    tool_call(&net, cwd.path());
    net.send(&net.key("claude", A), "body r1", "r1");
    let out: Value = serde_json::from_str(&stop(&net, cwd.path())).unwrap();
    assert_eq!(out, json!({"decision": "block", "reason": frame_of("r1")}));
    // The mail's Stop did not use up the nudge: the next one, with nothing waiting, has it.
    let out: Value = serde_json::from_str(&stop(&net, cwd.path())).unwrap();
    assert_eq!(
        out,
        json!({"decision": "block", "reason": lightbulb::hook::NUDGE})
    );
}

#[test]
fn a_session_without_tool_calls_is_not_nudged() {
    let net = Net::new();
    let cwd = started(&net);
    assert_eq!(stop(&net, cwd.path()), "");
}

#[test]
fn a_session_that_already_remembered_is_not_nudged() {
    let net = Net::new();
    let cwd = started(&net);
    tool_call(&net, cwd.path());
    remember_in(&net, &net.key("claude", A), "db", "postgres 16", "me");
    assert_eq!(stop(&net, cwd.path()), "");
}

/// A Stop that asks whether to nudge waits up to 250 ms for that answer, longer than the 10 ms
/// every other capture call gets: a daemon 100 ms slow (`LIGHTBULB_CONNECT_DEBUG_CAPTURE_DELAY_MS`
/// slows every capture op) still nudges, and the Stop's tokens are recorded.
#[test]
fn a_late_stop_answer_still_nudges() {
    let net = Net::with_env(&[("LIGHTBULB_CONNECT_DEBUG_CAPTURE_DELAY_MS", "100")]);
    let cwd = started(&net);
    tool_call(&net, cwd.path());
    let stop = json!({"hook_event_name": "Stop",
        "usage": {"input_tokens": 7, "output_tokens": 3}});
    let out: Value = serde_json::from_str(&claude_step(&net, "stop", stop, cwd.path())).unwrap();
    assert_eq!(
        out,
        json!({"decision": "block", "reason": lightbulb::hook::NUDGE})
    );
    let db = lightbulb::store::Store::open(&net.dir.path().join("state.db")).unwrap();
    let s = lightbulb::capture::sessions_since(&db, -1)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!((s.prompt_tokens, s.completion_tokens), (Some(7), Some(3)));
}
