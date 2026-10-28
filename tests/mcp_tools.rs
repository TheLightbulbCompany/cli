//! `mcp` driven over stdio as an agent drives it: the built binary, JSON-RPC lines.
//! The calling agent is pinned with LIGHTBULB_CONNECT_HOOK_CALLER (debug builds only),
//! since the test runner is not an agent.

use lightbulb::mail::MAX_HOPS;
use lightbulb::{os, paths};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Lines, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

const EXE: &str = env!("CARGO_BIN_EXE_lightbulb");
const NOTE: &str = "unread message(s) for this session; they arrive at your next step.";

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

    fn register(&self, provider: &str, conversation: &str, p: &os::Process) -> String {
        let r = self.call(
            json!({"id": 1, "op": "register", "session": {"provider": provider,
            "conversation": conversation, "pid": p.pid, "birth_micros": p.birth_micros,
            "cwd": "/tmp"}}),
        );
        r["key"]
            .as_str()
            .unwrap_or_else(|| panic!("{r}"))
            .to_string()
    }

    fn send(&self, from: &str, to: &str, text: &str, rid: &str) {
        let r = self.call(json!({"id": 1, "op": "send", "from": from, "to": to,
            "text": text, "request_id": rid}));
        assert_eq!(r["receipt"]["state"], "stored", "{r}");
    }

    /// The MCP server, run as the agent process `as_`.
    fn mcp(&self, as_: &os::Process) -> Mcp {
        self.mcp_named(as_, None)
    }

    /// `mcp`, with `--server-name` when `name` is given.
    fn mcp_named(&self, as_: &os::Process, name: Option<&str>) -> Mcp {
        let mut child = Command::new(EXE)
            .arg("mcp")
            .env("LIGHTBULB_CONNECT_DEBUG_NO_TELEMETRY", "1")
            .args(name.map(|n| ["--server-name", n]).into_iter().flatten())
            .env(paths::HOME_ENV, self.dir.path())
            .env("LIGHTBULB_CONNECT_CLAUDE_HOME", self.claude.path())
            .env(
                "LIGHTBULB_CONNECT_HOOK_CALLER",
                format!("{}:{}", as_.pid, as_.birth_micros),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut m = Mcp {
            stdin: child.stdin.take().unwrap(),
            lines: BufReader::new(child.stdout.take().unwrap()).lines(),
            child,
            next: 1,
            seen: vec![],
        };
        let init = m.rpc(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}}),
        );
        assert_eq!(
            init["result"]["serverInfo"]["name"],
            name.unwrap_or("lightbulb"),
            "{init}"
        );
        assert_eq!(init["result"]["capabilities"]["tools"]["listChanged"], true);
        m.write(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        m
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        let _ = UnixStream::connect(self.dir.path().join(paths::SOCKET))
            .and_then(|mut s| s.write_all(b"{\"id\":0,\"op\":\"shutdown\"}\n"));
        let _ = self.daemon.wait();
    }
}

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next: u64,
    /// What the server sent that answered no request (notifications).
    seen: Vec<Value>,
}

/// A tool call's outcome: whether it is an error, and its text.
struct Out {
    error: bool,
    text: String,
}

impl Out {
    /// The JSON before any trailing note.
    fn json(&self) -> Value {
        let body = self.text.split("\n\n").next().unwrap();
        serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {}", self.text))
    }
}

impl Mcp {
    fn write(&mut self, v: Value) {
        writeln!(self.stdin, "{v}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let line = self.lines.next().expect("server closed").unwrap();
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == id {
                return v;
            }
            self.seen.push(v);
        }
    }

    fn tool_meta(&mut self, name: &str, args: Value, meta: Value) -> Out {
        let r = self.rpc(
            "tools/call",
            json!({"name": name, "arguments": args, "_meta": meta}),
        );
        let res = &r["result"];
        Out {
            error: res["isError"] == true,
            text: res["content"][0]["text"]
                .as_str()
                .unwrap_or_else(|| panic!("{r}"))
                .to_string(),
        }
    }

    fn tool(&mut self, name: &str, args: Value) -> Out {
        self.tool_meta(name, args, json!({}))
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Another live process for a session to belong to; killed on drop.
struct Other(Child, os::Process);

impl Other {
    fn new() -> Other {
        let child = Command::new("sleep").arg("60").spawn().unwrap();
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

/// A live process whose executable is named `opencode` (this engine's binary copied under that
/// name, a hook waiting for its stdin to end), for the agent above an OpenCode MCP server;
/// killed on drop.
/// The temp dir holds its executable: kept for the process's lifetime.
struct FakeOpencode(Child, os::Process, #[allow(dead_code)] tempfile::TempDir);

impl FakeOpencode {
    fn new() -> FakeOpencode {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("opencode");
        // A copy, not a hard link: macOS may name a hard-linked running image by its other link.
        std::fs::copy(EXE, &exe).unwrap();
        let mut child = Command::new(&exe)
            .args(["hook", "none", "none"])
            .env(paths::HOME_ENV, dir.path())
            .env("LIGHTBULB_CONNECT_DEBUG_NO_TELEMETRY", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Until its exec lands, the child's executable is still this test's.
        for _ in 0..50 {
            let p = os::process(child.id()).unwrap();
            if p.executable.file_name().is_some_and(|n| n == "opencode") {
                return FakeOpencode(child, p, dir);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("the fake opencode never ran");
    }
}

impl Drop for FakeOpencode {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// OpenCode 2.x names its session in every call's `_meta`: among one OpenCode process's
/// sessions that picks the source, and a session no hook has registered yet is registered
/// from the MCP side (to that OpenCode process) on its first call.
#[test]
fn opencode_calls_pick_and_register_their_session_from_meta() {
    let net = Net::new();
    let oc = FakeOpencode::new();
    let a = net.register("opencode", "ses_a", &oc.1);
    let b = net.register("opencode", "ses_b", &oc.1);
    let mut m = net.mcp(&oc.1);
    let this = |v: &Value| -> Vec<String> {
        v["sessions"]
            .as_array()
            .unwrap_or_else(|| panic!("{v}"))
            .iter()
            .filter(|s| s["is_this_session"] == true)
            .map(|s| s["session_id"].as_str().unwrap().to_string())
            .collect()
    };
    let meta = |id: &str| json!({"ai.opencode/sessionID": id});
    assert_eq!(
        m.tool("find_sessions", json!({})).text,
        r#"{"error":"source_ambiguous"}"#
    );
    for key in [&b, &a] {
        let id = key.rsplit(':').next().unwrap();
        let v = m.tool_meta("find_sessions", json!({}), meta(id)).json();
        assert_eq!(this(&v), std::slice::from_ref(key), "{v}");
    }

    // An OpenCode process with no session yet: its first call registers the one it names, to
    // that process. An id that is no session id is refused first and registers nothing.
    let oc = FakeOpencode::new();
    let mut m = net.mcp(&oc.1);
    let before = net.call(json!({"id": 1, "op": "sessions"}))["sessions"].clone();
    for bad in ["a b", "", "x/y", &"s".repeat(129)] {
        let out = m.tool_meta("find_sessions", json!({}), meta(bad));
        assert_eq!(out.text, r#"{"error":"source_unidentified"}"#, "{bad:?}");
    }
    assert_eq!(
        net.call(json!({"id": 1, "op": "sessions"}))["sessions"],
        before
    );
    let v = m
        .tool_meta("find_sessions", json!({}), meta("ses_new"))
        .json();
    let new = this(&v);
    assert_eq!(new.len(), 1, "{v}");
    assert!(new[0].ends_with("/opencode:ses_new"), "{v}");
    let row = net.call(json!({"id": 1, "op": "sessions"}))["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == new[0].as_str())
        .cloned()
        .unwrap();
    assert_eq!(row["pid"], oc.1.pid);
    assert_eq!(row["birth_micros"], oc.1.birth_micros);
}

/// A subagent's tool call names its child session, which no hook registers (the plugin skips
/// child sessions): it never gets a row of its own. With one session on the OpenCode process
/// it is that session's call; with several it is unidentified.
#[test]
fn an_opencode_subagent_call_registers_no_session() {
    let net = Net::new();
    let oc = FakeOpencode::new();
    let parent = net.register("opencode", "ses_parent", &oc.1);
    let mut m = net.mcp(&oc.1);
    let rows = || net.call(json!({"id": 1, "op": "sessions"}))["sessions"].clone();
    let meta = json!({"ai.opencode/sessionID": "ses_child"});
    let v = m.tool_meta("find_sessions", json!({}), meta.clone()).json();
    let this: Vec<&Value> = v["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .filter(|s| s["is_this_session"] == true)
        .collect();
    assert_eq!(this.len(), 1, "{v}");
    assert_eq!(this[0]["session_id"], parent.as_str());
    assert_eq!(rows().as_array().map(Vec::len), Some(1), "{}", rows());

    net.register("opencode", "ses_other", &oc.1);
    let out = m.tool_meta("find_sessions", json!({}), meta);
    assert_eq!(out.text, r#"{"error":"source_unidentified"}"#);
    assert_eq!(rows().as_array().map(Vec::len), Some(2), "{}", rows());
}

/// The OpenCode meta key means nothing to another agent: it never registers a session for it.
#[test]
fn a_non_opencode_caller_with_opencode_meta_is_unaffected() {
    let net = Net::new();
    let claude = Other::new();
    let key = net.register("claude", "c1", &claude.1);
    let mut m = net.mcp(&claude.1);
    let v = m
        .tool_meta(
            "find_sessions",
            json!({}),
            json!({"ai.opencode/sessionID": "ses_x"}),
        )
        .json();
    let this: Vec<&Value> = v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["is_this_session"] == true)
        .collect();
    assert_eq!(this.len(), 1, "{v}");
    assert_eq!(this[0]["session_id"], key.as_str());
    // With no session of its own, it stays unidentified and nothing is registered.
    let lone = Other::new();
    let mut m = net.mcp(&lone.1);
    let out = m.tool_meta(
        "find_sessions",
        json!({}),
        json!({"ai.opencode/sessionID": "ses_y"}),
    );
    assert_eq!(out.text, r#"{"error":"source_unidentified"}"#);
    let sessions = net.call(json!({"id": 1, "op": "sessions"}))["sessions"].clone();
    assert_eq!(sessions.as_array().map(Vec::len), Some(1), "{sessions}");
}

fn me() -> os::Process {
    os::process(std::process::id()).unwrap()
}

#[test]
fn send_message_from_claude_to_codex_returns_stored_and_receipt_follows() {
    let net = Net::new();
    let codex = Other::new();
    let from = net.register("claude", "c1", &me());
    let to = net.register("codex", "t1", &codex.1);
    let mut m = net.mcp(&me());

    let out = m.tool(
        "send_message",
        json!({"request_id": "r1", "text": "hi codex", "session_id": to}),
    );
    assert!(!out.error, "{}", out.text);
    assert_eq!(out.json()["state"], "stored");
    assert_eq!(out.json()["request_id"], "r1");

    let out = m.tool("message_receipt", json!({"request_id": "r1"}));
    assert_eq!(out.json()["state"], "stored");

    // The codex session takes it; the sender's receipt follows.
    let r = net.call(json!({"id": 1, "op": "inbox", "key": to}));
    assert_eq!(r["messages"][0]["from"], from.as_str());
    assert_eq!(r["messages"][0]["text"], "hi codex");
    let out = m.tool("message_receipt", json!({"request_id": "r1"}));
    assert_eq!(out.json()["state"], "delivered");
}

#[test]
fn check_inbox_waits_until_mail_arrives() {
    let net = Net::new();
    let key = net.register("claude", "c1", &me());
    let mut m = net.mcp(&me());

    let started = Instant::now();
    std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_secs(1));
            net.send("x/codex:a", &key, "late news", "r9");
        });
        let out = m.tool("check_inbox", json!({"wait_secs": 3}));
        let took = started.elapsed();
        assert!(!out.error, "{}", out.text);
        assert!(
            took >= Duration::from_millis(900) && took < Duration::from_millis(2500),
            "{took:?}"
        );
        let msgs = out.json()["messages"].clone();
        assert_eq!(msgs.as_array().unwrap().len(), 1, "{msgs}");
        let msg = &msgs[0];
        assert_eq!(msg["request_id"], "r9");
        assert_eq!(msg["from"], "x/codex:a");
        assert_eq!(msg["text"], "late news", "{msg}");
        let framed = msg["framed"].as_str().unwrap();
        assert!(
            framed.contains("late news") && framed.contains("reply_to=\"r9\""),
            "{msg}"
        );
        for f in ["id", "thread", "sent_at"] {
            assert!(!msg[f].is_null(), "{f} missing: {msg}");
        }
    });
    let out = m.tool("check_inbox", json!({"wait_secs": 601}));
    assert!(out.error);
    assert_eq!(out.json(), json!({"error": "bad_request"}));
}

#[test]
fn source_unidentified_when_no_session_matches() {
    let net = Net::new();
    net.register("claude", "someone-else", &Other::new().1);
    let mut m = net.mcp(&me());
    for (tool, args) in [
        ("find_sessions", json!({})),
        (
            "send_message",
            json!({"request_id": "r1", "text": "t", "session_id": "x/claude:y"}),
        ),
        ("message_receipt", json!({"request_id": "r1"})),
        ("check_inbox", json!({})),
    ] {
        let out = m.tool(tool, args);
        assert!(out.error, "{tool}");
        assert_eq!(out.text, r#"{"error":"source_unidentified"}"#, "{tool}");
    }
}

#[test]
fn tool_results_carry_the_unread_note_for_pull_tier() {
    let net = Net::new();
    let key = net.register("claude", "c1", &me());
    let mut m = net.mcp(&me());
    let out = m.tool("find_sessions", json!({}));
    assert!(!out.text.contains(NOTE), "no mail, no note: {}", out.text);

    net.send("x/codex:a", &key, "one", "r1");
    net.send("x/codex:a", &key, "two", "r2");
    let out = m.tool("find_sessions", json!({}));
    assert!(
        out.text
            .ends_with("\n\n2 unread message(s) for this session; they arrive at your next step."),
        "{}",
        out.text
    );
    // The note only counts: the mail still waits.
    let out = m.tool("check_inbox", json!({}));
    assert_eq!(out.json()["messages"].as_array().unwrap().len(), 2);
    assert!(!out.text.contains(NOTE));
}

#[test]
fn find_sessions_marks_is_this_session() {
    let net = Net::new();
    let other = Other::new();
    let mine = net.register("claude", "c1", &me());
    let theirs = net.register("codex", "t1", &other.1);
    let mut m = net.mcp(&me());

    let out = m.tool("find_sessions", json!({}));
    let v = out.json();
    assert_eq!(v["total"], 2, "{v}");
    let find = |key: &str| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["session_id"] == key)
            .cloned()
            .unwrap_or_else(|| panic!("{key} not in {v}"))
    };
    let (a, b) = (find(&mine), find(&theirs));
    assert_eq!(
        (a["is_this_session"].clone(), b["is_this_session"].clone()),
        (json!(true), json!(false))
    );
    assert_eq!(b["agent"], "codex");
    assert_eq!(b["status"], "idle");
    assert_eq!(b["cwd"], "/tmp");
    assert_eq!(
        b["person_verified"], true,
        "this computer, as its label says"
    );
    // `who`'s name for each, and a query by it finds just that one.
    let name = b["name"]
        .as_str()
        .unwrap_or_else(|| panic!("no name in {b}"))
        .to_string();
    assert!(name.starts_with("codex-"), "{name}");
    let by_name = m.tool("find_sessions", json!({"query": name})).json();
    assert_eq!(by_name["total"], 1, "{by_name}");
    assert_eq!(by_name["sessions"][0]["session_id"], theirs.as_str());
    assert_eq!(b["online"], true);
    assert_eq!(b["can_send"], true);
    assert_eq!(b["needs_owner_ok"], false);
    assert_eq!(b["tier"], "next_step");
    assert!(b["computer"].as_str().is_some_and(|c| !c.is_empty()));
    assert!(b["person"].as_str().is_some_and(|c| !c.is_empty()));

    let v = m
        .tool("find_sessions", json!({"query": "codex", "limit": 5}))
        .json();
    assert_eq!(v["total"], 1, "{v}");
    let v = m.tool("find_sessions", json!({"limit": 1})).json();
    assert_eq!(
        (v["total"].clone(), v["sessions"].as_array().unwrap().len()),
        (json!(2), 1)
    );
}

#[test]
fn codex_calls_pick_their_thread_from_meta() {
    let net = Net::new();
    let codex = Other::new();
    let t1 = net.register("codex", "t1", &codex.1);
    net.register("codex", "t2", &codex.1);
    let mut m = net.mcp(&codex.1);

    let out = m.tool("find_sessions", json!({}));
    assert_eq!(out.text, r#"{"error":"source_ambiguous"}"#);
    let v = m
        .tool_meta("find_sessions", json!({}), json!({"threadId": "t1"}))
        .json();
    let this: Vec<&Value> = v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["is_this_session"] == true)
        .collect();
    assert_eq!(this.len(), 1, "{v}");
    assert_eq!(this[0]["session_id"], t1.as_str());
}

#[test]
fn a_failed_push_brings_the_unread_note_back() {
    let net = Net::new();
    // This process as a Claude session whose messaging socket has no listener.
    let sock = net.claude.path().join("dead.sock");
    drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
    let sessions = net.claude.path().join(".claude/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let entry = json!({"pid": me().pid, "sessionId": "c9", "cwd": "/tmp",
        "startedAt": lightbulb::os::started_ms(me().pid).unwrap(), "status": "idle", "messagingSocketPath": sock});
    std::fs::write(sessions.join("a.json"), entry.to_string()).unwrap();
    let short = net.call(json!({"id": 1, "op": "status"}))["member_key_short"]
        .as_str()
        .unwrap()
        .to_string();
    let key = format!("{short}/claude:c9");
    let mut m = net.mcp(&me());
    let tier =
        |m: &mut Mcp| m.tool("find_sessions", json!({})).json()["sessions"][0]["tier"].clone();
    assert_eq!(tier(&mut m), "push");

    net.send("x/codex:a", &key, "one", "r1");
    let deadline = Instant::now() + Duration::from_secs(5);
    while tier(&mut m) != "next_step" && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = m.tool("find_sessions", json!({}));
    assert_eq!(
        out.json()["sessions"][0]["tier"],
        "next_step",
        "{}",
        out.text
    );
    assert!(out.text.ends_with(&format!("\n\n1 {NOTE}")), "{}", out.text);
}

#[test]
fn the_server_advertises_its_installed_name() {
    let net = Net::new();
    // mcp_named asserts serverInfo.name on initialize.
    net.mcp_named(&me(), Some("work-tools"));
}

#[test]
fn send_message_with_reply_to_alone_answers_the_sender() {
    let net = Net::new();
    let key = net.register("claude", "c1", &me());
    // A name mailbox on this computer: a reply under another prefix would leave it.
    let short = net.call(json!({"id": 1, "op": "status"}))["member_key_short"]
        .as_str()
        .unwrap()
        .to_string();
    let desk = format!("{short}/desk");
    net.send(&desk, &key, "question", "q1");
    let mut m = net.mcp(&me());
    assert!(!m.tool("check_inbox", json!({})).error);
    let out = m.tool(
        "send_message",
        json!({"request_id": "a1", "text": "answer", "reply_to": "q1"}),
    );
    assert!(!out.error, "{}", out.text);
    let r = net.call(json!({"id": 1, "op": "inbox", "key": desk}));
    assert_eq!(r["messages"][0]["text"], "answer", "{r}");
    // Answering it marked the question read.
    let r = net.call(json!({"id": 1, "op": "receipt", "from": desk, "request_id": "q1"}));
    assert_eq!(r["receipt"]["state"], "read", "{r}");
    // Neither a recipient nor a reply_to: refused.
    let out = m.tool("send_message", json!({"request_id": "a2", "text": "t"}));
    assert!(out.error, "{}", out.text);
}

#[test]
fn a_tool_call_after_a_delivery_marks_it_read() {
    let net = Net::new();
    let key = net.register("claude", "c1", &me());
    net.send("x/codex:a", &key, "hello", "r1");
    let mut m = net.mcp(&me());
    assert!(!m.tool("check_inbox", json!({})).error);
    let receipt = || {
        net.call(json!({"id": 1, "op": "receipt", "from": "x/codex:a", "request_id": "r1"}))
            ["receipt"]["state"]
            .clone()
    };
    assert_eq!(receipt(), "delivered");
    std::thread::sleep(Duration::from_millis(1100));
    assert!(!m.tool("find_sessions", json!({})).error);
    assert_eq!(receipt(), "read");
}

// ---- across members ---------------------------------------------------------------------

mod common;

/// The MCP server on `e`'s daemon, run as the agent process `as_`.
fn mcp_on(e: &common::Env, as_: &os::Process) -> Mcp {
    mcp_at(e, as_, "http://127.0.0.1:9")
}

/// `mcp_on`, its Lightbulb API at `api`.
fn mcp_at(e: &common::Env, as_: &os::Process, api: &str) -> Mcp {
    mcp_exe(e, as_, api, std::path::Path::new(EXE))
}

/// `mcp_at`, run from the binary at `exe`.
fn mcp_exe(e: &common::Env, as_: &os::Process, api: &str, exe: &std::path::Path) -> Mcp {
    let mut c = e.base_cmd(Command::new(exe));
    c.arg("mcp")
        .env("LIGHTBULB_API", api)
        .env("LIGHTBULB_CONNECT_DEBUG_TOOLS_POLL_MS", "200")
        .env(
            "LIGHTBULB_CONNECT_HOOK_CALLER",
            format!("{}:{}", as_.pid, as_.birth_micros),
        );
    let mut child = c
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut m = Mcp {
        stdin: child.stdin.take().unwrap(),
        lines: BufReader::new(child.stdout.take().unwrap()).lines(),
        child,
        next: 1,
        seen: vec![],
    };
    m.rpc(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}}),
    );
    m.write(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    m
}

/// Registers `p` on `e` as session `<provider>:<conv>` in `cwd`; its key.
fn register_on(e: &common::Env, provider: &str, conv: &str, p: &os::Process, cwd: &str) -> String {
    let r = e.ask(
        json!({"id": 1, "op": "register", "session": {"provider": provider,
        "conversation": conv, "pid": p.pid, "birth_micros": p.birth_micros, "cwd": cwd}}),
    );
    r["key"]
        .as_str()
        .unwrap_or_else(|| panic!("{r}"))
        .to_string()
}

#[test]
fn find_sessions_lists_teammates_with_person_and_computer() {
    let (a, b) = common::pair(&[]);
    let codex = Other::new();
    let mine = register_on(&a, "claude", "c1", &me(), "/tmp");
    let theirs = register_on(&b, "codex", "b1", &codex.1, "/tmp/api");
    common::until(15, "A sees B's session", || common::sees(&a, &theirs));
    let mut m = mcp_on(&a, &me());
    let v = m.tool("find_sessions", json!({})).json();
    let find = |key: &str| {
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["session_id"] == key)
            .cloned()
            .unwrap_or_else(|| panic!("{key} not in {v}"))
    };
    let (own, bob) = (find(&mine), find(&theirs));
    assert_eq!(own["is_this_session"], true, "{own}");
    assert_eq!(own["computer"], "mac", "{own}");
    assert_eq!(
        (&bob["person"], &bob["computer"], &bob["person_verified"]),
        (&json!("bob"), &json!("mbp"), &json!(false)),
        "{bob}"
    );
    assert_eq!(bob["needs_owner_ok"], true, "{bob}");
    assert_eq!(bob["is_this_session"], false, "{bob}");
    assert_eq!(bob["cwd"], "", "a teammate's path stays on their computer");
    // The query reaches teammates too: by person, by computer.
    for q in ["bob", "mbp"] {
        let v = m.tool("find_sessions", json!({"query": q})).json();
        assert_eq!(v["total"], 1, "{q}: {v}");
    }
}

#[test]
fn send_message_to_teammate_says_where_it_waits() {
    let (a, b) = (common::Env::new(), common::Env::new());
    // One team: members trust each other, so the message is stored on bob's computer.
    common::seed_team(&a, &b);
    common::start_pair(&a, &b, &common::computer("mac"), &[]);
    register_on(&a, "claude", "c1", &me(), "/tmp");
    let to = format!("{}/codex:b1", b.short());
    let mut m = mcp_on(&a, &me());

    let out = m.tool(
        "send_message",
        json!({"request_id": "r1", "text": "did the token format change?", "session_id": to}),
    );
    assert!(!out.error, "{}", out.text);
    let v = out.json();
    assert_eq!(v["state"], "stored", "{v}");
    assert_eq!(v["note"], "stored on mbp.", "{v}");
    let from = format!("{}/claude:c1", a.short());
    b.take_one(&to);
    common::until(10, "r1 delivered", || a.state(&from, "r1") == "delivered");

    // Groups stay refused.
    let out = m.tool(
        "send_message",
        json!({"request_id": "g1", "text": "x", "session_id": "*"}),
    );
    assert!(out.error);
    assert_eq!(out.json(), json!({"error": "group_send_not_allowed"}));

    // Bob's computer goes offline: queued, retried for a day.
    b.ask(json!({"id": 1, "op": "shutdown"}));
    common::until(10, "B offline", || a.status()["peers"]["online"] == 0);
    let v = m
        .tool(
            "send_message",
            json!({"request_id": "r3", "text": "x", "session_id": to}),
        )
        .json();
    assert_eq!(v["state"], "queued", "{v}");
    let note = v["note"].as_str().unwrap_or_else(|| panic!("{v}"));
    assert!(
        note.starts_with("queued: mbp is offline. Will retry until ") && note.ends_with('.'),
        "{note}"
    );
    assert_eq!(v["online"], false, "{v}");

    // The 12-per-10-minutes limit, in words.
    for i in 4..=13 {
        let rid = format!("r{i}");
        let v = m
            .tool(
                "send_message",
                json!({"request_id": rid, "text": "x", "session_id": to}),
            )
            .json();
        assert_eq!(v["state"], "queued", "{v}");
    }
    let v = m
        .tool(
            "send_message",
            json!({"request_id": "r14", "text": "x", "session_id": to}),
        )
        .json();
    assert_eq!(v["reason"], "loop", "{v}");
    assert_eq!(
        v["note"], "loop limit: 12 messages in 10 minutes to this session",
        "{v}"
    );
}

#[test]
fn check_inbox_record_names_person_and_computer() {
    let (a, b) = common::pair(&[]);
    let mine = register_on(&a, "claude", "c1", &me(), "/tmp");
    let sb = format!("{}/codex:b1", b.short());
    let r = b.send(&sb, &mine, "the token format changed", "q1", None);
    assert_eq!(r["state"], "queued", "{r}");
    let mut m = mcp_on(&a, &me());
    let out = m.tool("check_inbox", json!({"wait_secs": 10}));
    assert!(!out.error, "{}", out.text);
    let msg = out.json()["messages"][0].clone();
    assert_eq!(msg["from"], sb.as_str(), "{msg}");
    assert_eq!(msg["label"], "bob's Codex (not verified)", "{msg}");
    assert_eq!(msg["person"], "bob", "{msg}");
    assert_eq!(msg["person_verified"], false, "{msg}");
    assert_eq!(msg["computer"], "mbp", "{msg}");
    assert_eq!(msg["text"], "the token format changed", "{msg}");
    for f in ["id", "request_id", "thread", "sent_at"] {
        assert!(!msg[f].is_null(), "{f} missing: {msg}");
    }
}

#[test]
fn an_online_teammate_that_has_not_acked_has_not_confirmed_yet() {
    // A drops every ack it is sent: B stays online but never confirms.
    let (a, b) = common::pair(&[("LIGHTBULB_CONNECT_DEBUG_DROP_ACKS", "1".into())]);
    register_on(&a, "claude", "c1", &me(), "/tmp");
    let to = format!("{}/codex:b1", b.short());
    let mut m = mcp_on(&a, &me());
    let v = m
        .tool(
            "send_message",
            json!({"request_id": "r1", "text": "x", "session_id": to}),
        )
        .json();
    assert_eq!(
        (&v["state"], &v["online"]),
        (&json!("queued"), &json!(true)),
        "{v}"
    );
    let note = v["note"].as_str().unwrap_or_else(|| panic!("{v}"));
    assert!(
        note.starts_with("queued: mbp has not confirmed yet. Will retry until "),
        "{note}"
    );
}

#[test]
fn loop_refusals_name_their_limit_here_and_on_a_teammate() {
    let (a, b) = common::pair(&[]);
    let mine = register_on(&a, "claude", "c1", &me(), "/tmp");
    let x = format!("{}/codex:x", a.short());
    let mut m = mcp_on(&a, &me());

    // Here: the agent reply past MAX_HOPS in a chain.
    a.send(&mine, &x, "x", "h0", None);
    for i in 1..MAX_HOPS {
        let (from, to) = if i % 2 == 1 { (&x, &mine) } else { (&mine, &x) };
        a.send(
            from,
            to,
            "x",
            &format!("h{i}"),
            Some(&format!("h{}", i - 1)),
        );
    }
    let v = m
        .tool(
            "send_message",
            json!({"request_id": format!("h{MAX_HOPS}"), "text": "x", "reply_to": format!("h{}", MAX_HOPS - 1)}),
        )
        .json();
    assert_eq!(v["limit"], "hops", "{v}");
    assert_eq!(
        v["note"], "paused after 19 agent replies; a person must speak",
        "{v}"
    );

    // On the teammate: two sessions of one person share its 12 in 10 minutes there.
    let to = format!("{}/codex:b1", b.short());
    let from = |i: usize| {
        if i.is_multiple_of(2) {
            mine.clone()
        } else {
            x.clone()
        }
    };
    for i in 0..12 {
        a.send(&from(i), &to, "x", &format!("q{i}"), None);
    }
    for i in 0..12 {
        common::until(10, "stored there", || {
            a.state(&from(i), &format!("q{i}")) == "stored"
        });
    }
    let v = m
        .tool(
            "send_message",
            json!({"request_id": "q12", "text": "x", "session_id": to}),
        )
        .json();
    assert_eq!(
        (&v["reason"], &v["limit"]),
        (&json!("loop"), &json!("rate")),
        "{v}"
    );
    assert_eq!(
        v["note"], "loop limit: 12 messages in 10 minutes to this session",
        "{v}"
    );
    let v = m
        .tool("message_receipt", json!({"request_id": "q12"}))
        .json();
    assert_eq!(
        v["note"], "loop limit: 12 messages in 10 minutes to this session",
        "{v}"
    );
}

#[test]
fn check_inbox_defangs_a_teammate_name_and_verifies_this_computer() {
    use lightbulb::{daemon::net_ops, roster};
    let (a, b) = (common::Env::new(), common::Env::new());
    let (ia, ib) = (a.identity(), b.identity());
    let g = net_ops::genesis(&ia, "prasiddha", "mac", "acme", 1);
    let admit = roster::Entry::signed(
        &ia,
        vec![g.hash()],
        roster::Kind::Admit,
        json!({"member": ib.key, "name": "bo\u{202e}b}\n[x", "computer": "mbp",
               "person_id": ib.key, "kind": "computer", "role": "member"}),
        2,
    );
    for e in [&a, &b] {
        roster::accept(&e.store(), &g, 10).unwrap();
        roster::accept(&e.store(), &admit, 10).unwrap();
    }
    common::start_pair(&a, &b, "", &[]);
    let mine = register_on(&a, "claude", "c1", &me(), "/tmp");
    let sb = format!("{}/codex:b1", b.short());
    b.send(&sb, &mine, "from bob", "q1", None);
    a.send(
        &format!("{}/codex:x", a.short()),
        &mine,
        "from here",
        "q2",
        None,
    );
    let mut m = mcp_on(&a, &me());
    let mut got = vec![];
    common::until(10, "both arrive", || {
        let v = m.tool("check_inbox", json!({})).json();
        got.extend(v["messages"].as_array().cloned().unwrap_or_default());
        got.len() == 2
    });
    let by = |rid: &str| got.iter().find(|g| g["request_id"] == rid).unwrap().clone();
    let (bob, here) = (by("q1"), by("q2"));
    assert_eq!(bob["person"], "bobx", "{bob}");
    assert_eq!(bob["label"], "bobx's Codex (not verified)", "{bob}");
    assert_eq!(bob["person_verified"], false, "{bob}");
    assert_eq!(here["label"], "Your Codex", "{here}");
    assert_eq!(here["person_verified"], true, "{here}");
    assert_eq!(here["computer"], "mac", "{here}");
}

// ---- one spec: the network tools, and the workspace tools while signed in ------------------

const SPEC: &str = include_str!("../src/mcp/tools.json");

fn spec() -> Vec<Value> {
    serde_json::from_str(SPEC).unwrap()
}

fn names(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

impl Mcp {
    fn tools(&mut self) -> Vec<Value> {
        let r = self.rpc("tools/list", json!({}));
        r["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("{r}"))
            .clone()
    }
}

/// One request the stub API got.
#[derive(Clone, Debug)]
struct Req {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Req {
    fn header(&self, name: &str) -> &str {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("no {name} in {self:?}"))
    }
}

type Seen = std::sync::Arc<std::sync::Mutex<Vec<Req>>>;

/// A stub Lightbulb API answering `answer(method, path)`: its base URL, and what it saw.
fn stub(answer: fn(&str, &str) -> (u16, Value)) -> (String, Seen) {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/api/v1", listener.local_addr().unwrap());
    let seen: Seen = Default::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let mut s = s.unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut first = String::new();
            r.read_line(&mut first).unwrap();
            let mut parts = first.split(' ');
            let method = parts.next().unwrap_or_default().to_string();
            let path = parts.next().unwrap_or_default().to_string();
            let (mut headers, mut len) = (vec![], 0);
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h == "\r\n" {
                    break;
                }
                let (k, v) = h.trim_end().split_once(':').unwrap();
                let (k, v) = (k.to_lowercase(), v.trim().to_string());
                if k == "content-length" {
                    len = v.parse().unwrap();
                }
                headers.push((k, v));
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let (code, v) = answer(&method, &path);
            log.lock().unwrap().push(Req {
                method,
                path,
                headers,
                body: String::from_utf8(body).unwrap(),
            });
            let text = v.to_string();
            let _ = write!(
                s,
                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
        }
    });
    (base, seen)
}

fn api(method: &str, path: &str) -> (u16, Value) {
    let path = path.split('?').next().unwrap();
    match (method, path) {
        ("POST", "/api/v1/messaging/rooms") => (201, json!({"roomId": "dm-1", "kind": "dm"})),
        ("POST", p) if p.ends_with("/messages") => {
            let room = p.split('/').nth(5).unwrap();
            (
                201,
                json!({"roomId": room, "messageId": "m-1", "messageSeq": 7, "text": "x"}),
            )
        }
        ("GET", "/api/v1/local-sessions/device/receipt") => (200, json!({"state": "posted"})),
        ("GET", "/api/v1/local-sessions/device/rooms") => (200, json!({"rooms": [], "total": 0})),
        ("PATCH", _) => (403, json!({"detail": {"message": "Not your task."}})),
        _ => (404, json!({"detail": "Not Found"})),
    }
}

/// A signed-in computer named mac, its daemon up, this process its Claude session `c1`.
fn signed_in() -> common::Env {
    let e = common::Env::new();
    e.sign_in("user_1", None);
    e.daemon(false, &common::computer("mac"), &[]);
    register_on(&e, "claude", "c1", &me(), "/tmp");
    e
}

#[test]
fn tools_list_matches_tools_json() {
    let e = signed_in();
    let mut m = mcp_on(&e, &me());
    let shape = |t: &Value| json!({"name": t["name"], "description": t["description"], "inputSchema": t["inputSchema"]});
    let got: Vec<Value> = m.tools().iter().map(shape).collect();
    let want: Vec<Value> = spec().iter().map(shape).collect();
    assert_eq!(got, want);
    assert_eq!(names(&got).len(), 15);
}

#[test]
fn tools_json_flag_prints_the_spec() {
    // Printed before anything else runs: no daemon, no state read.
    let dir = tempfile::tempdir().unwrap();
    let o = Command::new(EXE)
        .args(["mcp", "--tools-json"])
        .env(paths::HOME_ENV, dir.path().join("none"))
        .output()
        .unwrap();
    assert!(!dir.path().join("none").exists());
    assert!(o.status.success());
    assert_eq!(o.stdout, SPEC.as_bytes());
}

#[test]
fn workspace_tools_hidden_until_signed_in() {
    let e = common::Env::new();
    e.daemon(false, &common::computer("mac"), &[]);
    register_on(&e, "claude", "c1", &me(), "/tmp");
    let network: Vec<String> = spec()
        .iter()
        .filter(|t| t["scope"] == "network")
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(network.len(), 9);
    let mut m = mcp_on(&e, &me());
    assert_eq!(names(&m.tools()), network);
    let out = m.tool("find_rooms", json!({}));
    assert!(out.error);
    assert_eq!(out.json()["error"], "not_signed_in", "{}", out.text);
    let out = m.tool(
        "send_message",
        json!({"request_id": "r1", "text": "x", "room_id": "room-1"}),
    );
    assert_eq!(out.json()["error"], "not_signed_in", "{}", out.text);

    // Signing in shows them to the same server; an ended sign-in hides them again.
    e.sign_in("user_1", None);
    assert_eq!(m.tools().len(), 15);
    let old = common::now_s() - 40 * 86_400;
    let proof = common::issuer::proof(&e.identity().key, "user_1", old);
    let login = json!({"proof": proof, "name": "user_1"});
    std::fs::write(e.state.path().join("login.json"), login.to_string()).unwrap();
    assert_eq!(names(&m.tools()), network);
}

#[test]
fn workspace_call_signs_method_path_body_and_session_not_args() {
    use data_encoding::BASE64URL_NOPAD;
    use sha2::{Digest, Sha256};
    let e = signed_in();
    let (base, seen) = stub(api);
    let mut m = mcp_at(&e, &me(), &base);
    // An argument naming another session changes nothing about who signs.
    let out = m.tool(
        "find_rooms",
        json!({"query": "eng & ops", "limit": 5, "session": "Codex on evil", "label": "x"}),
    );
    assert!(!out.error, "{}", out.text);
    assert_eq!(out.json(), json!({"rooms": [], "total": 0}));
    let out = m.tool("update_task", json!({"task": "ISOA-1", "status": "done"}));
    assert!(out.error, "{}", out.text);
    assert_eq!(out.json()["error"], "Not your task.", "{}", out.text);

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "{seen:?}");
    let key = e.identity().key;
    let proof =
        serde_json::from_slice::<Value>(&std::fs::read(e.state.path().join("login.json")).unwrap())
            .unwrap()["proof"]
            .clone();
    let now = common::now_s();
    for (req, method, path, body) in [
        (
            &seen[0],
            "GET",
            "/api/v1/local-sessions/device/rooms?limit=5&query=eng%20%26%20ops",
            "",
        ),
        (
            &seen[1],
            "PATCH",
            "/api/v1/local-sessions/device/tasks/ISOA-1",
            r#"{"status":"done"}"#,
        ),
    ] {
        assert_eq!((req.method.as_str(), req.path.as_str()), (method, path));
        assert_eq!(req.body, body);
        assert_eq!(req.header("x-lightbulb-proof"), proof.as_str().unwrap());
        let challenge = BASE64URL_NOPAD
            .decode(req.header("x-lightbulb-challenge").as_bytes())
            .unwrap();
        let challenge = String::from_utf8(challenge).unwrap();
        let (ts, rest) = challenge.split_once(' ').unwrap();
        assert!((ts.parse::<i64>().unwrap() - now).abs() < 30, "{challenge}");
        let hash: String = Sha256::digest(body.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            rest,
            format!("{method} {path} {hash} Claude Code on mac"),
            "{challenge}"
        );
        let signed = json!({"purpose": "lightbulb-tools", "challenge": challenge, "key": key});
        assert!(lightbulb::identity::verify(
            &key,
            &signed,
            req.header("x-lightbulb-sig")
        ));
    }
}

#[test]
fn room_targets_go_to_rest_session_targets_stay_p2p() {
    let e = signed_in();
    let codex = Other::new();
    let to = register_on(&e, "codex", "t1", &codex.1, "/tmp");
    let (base, seen) = stub(api);
    let mut m = mcp_at(&e, &me(), &base);

    // A session send stays on the network: the API never hears of it.
    let out = m.tool(
        "send_message",
        json!({"request_id": "s1", "text": "hi", "session_id": to}),
    );
    assert_eq!(out.json()["state"], "stored", "{}", out.text);
    let out = m.tool("message_receipt", json!({"request_id": "s1"}));
    assert_eq!(out.json()["state"], "stored", "{}", out.text);
    assert!(seen.lock().unwrap().is_empty());

    // A session send takes no thread or mentions; exactly one target.
    for bad in [
        json!({"request_id": "b1", "text": "x", "session_id": to, "mention_ids": ["u"]}),
        json!({"request_id": "b2", "text": "x", "session_id": to, "room_id": "r"}),
        json!({"request_id": "b3", "text": "x", "room_id": "r", "reply_to": "s1"}),
    ] {
        let out = m.tool("send_message", bad.clone());
        assert_eq!(out.json()["error"], "bad_request", "{bad}: {}", out.text);
    }

    // A room send posts to the room, a thread reply and mentions with it.
    let out = m.tool(
        "send_message",
        json!({"request_id": "r1", "text": "ship it", "room_id": "room-1",
            "reply_to_message_id": "m-0", "mention_ids": ["user_2", "agent-9"]}),
    );
    assert!(!out.error, "{}", out.text);
    assert_eq!(
        out.json(),
        json!({"roomId": "room-1", "messageId": "m-1", "messageSeq": 7, "state": "posted"})
    );
    // A person send opens the DM, then posts there.
    let out = m.tool(
        "send_message",
        json!({"request_id": "p1", "text": "hey", "person_id": "user_2"}),
    );
    assert_eq!(out.json()["roomId"], "dm-1", "{}", out.text);
    // Their receipts are read from the rooms they went to.
    let out = m.tool("message_receipt", json!({"request_id": "p1"}));
    assert_eq!(out.json()["state"], "posted", "{}", out.text);

    let seen = seen.lock().unwrap().clone();
    let calls: Vec<(String, String)> = seen
        .iter()
        .map(|r| {
            (
                r.method.clone(),
                r.path.split('?').next().unwrap().to_string(),
            )
        })
        .collect();
    let call = |m: &str, p: &str| (m.to_string(), p.to_string());
    assert_eq!(
        calls,
        [
            call("POST", "/api/v1/messaging/rooms/room-1/messages"),
            call("POST", "/api/v1/messaging/rooms"),
            call("POST", "/api/v1/messaging/rooms/dm-1/messages"),
            call("GET", "/api/v1/local-sessions/device/receipt"),
        ]
    );
    let post: Value = serde_json::from_str(&seen[0].body).unwrap();
    assert_eq!(post["text"], "ship it");
    assert_eq!(post["replyToMessageId"], "m-0");
    assert_eq!(
        post["mentions"],
        json!([{"kind": "human", "id": "user_2"}, {"kind": "agent", "id": "agent-9"}])
    );
    let uuid = post["clientMessageId"].as_str().unwrap();
    assert_eq!((uuid.len(), uuid.matches('-').count()), (36, 4), "{uuid}");
    let dm: Value = serde_json::from_str(&seen[1].body).unwrap();
    assert_eq!(dm["kind"], "dm");
    assert_eq!(dm["members"][0]["id"], "user_2");
    assert_eq!(dm["members"][0]["kind"], "human");
    let dm_post: Value = serde_json::from_str(&seen[2].body).unwrap();
    assert!(
        seen[3].path.contains("room_id=dm-1")
            && seen[3].path.contains(&format!(
                "request_id={}",
                dm_post["clientMessageId"].as_str().unwrap()
            )),
        "{}",
        seen[3].path
    );
}

#[test]
fn the_tool_list_changes_when_the_sign_in_does() {
    let e = common::Env::new();
    e.daemon(false, &common::computer("mac"), &[]);
    register_on(&e, "claude", "c1", &me(), "/tmp");
    let mut m = mcp_on(&e, &me());
    let changes = |m: &mut Mcp| {
        m.seen
            .drain(..)
            .filter(|v| v["method"] == "notifications/tools/list_changed")
            .count()
    };
    let wait = || std::thread::sleep(Duration::from_millis(800));
    assert_eq!(m.tools().len(), 9);
    // A login with no request after it: the timer tells the client.
    e.sign_in("user_1", None);
    wait();
    assert_eq!(m.tools().len(), 15);
    assert_eq!(changes(&mut m), 1);
    // A logout: told once, by the next call at the latest.
    std::fs::remove_file(e.state.path().join("login.json")).unwrap();
    assert!(!m.tool("find_sessions", json!({})).error);
    assert_eq!(changes(&mut m), 1);
    assert_eq!(m.tools().len(), 9);
    // A sign-in that ends.
    e.sign_in("user_1", None);
    wait();
    m.tools();
    assert_eq!(changes(&mut m), 1);
    let old = common::now_s() - 40 * 86_400;
    let proof = common::issuer::proof(&e.identity().key, "user_1", old);
    let login = json!({"proof": proof, "name": "user_1"});
    std::fs::write(e.state.path().join("login.json"), login.to_string()).unwrap();
    wait();
    assert_eq!(m.tools().len(), 9);
    assert_eq!(changes(&mut m), 1);
    // Nothing changed: nothing said.
    wait();
    m.tools();
    assert_eq!(changes(&mut m), 0);
}

#[test]
fn a_server_whose_binary_was_replaced_says_so_and_sends_nothing() {
    let e = signed_in();
    let (base, seen) = stub(api);
    let bin = tempfile::tempdir().unwrap();
    let exe = bin.path().join("lightbulb");
    std::fs::copy(EXE, &exe).unwrap();
    let mut m = mcp_exe(&e, &me(), &base, &exe);
    let out = m.tool("find_rooms", json!({}));
    assert!(!out.error, "{}", out.text);

    // `lightbulb update` renames the new binary over the running one.
    let new = bin.path().join("lightbulb.new");
    std::fs::copy(EXE, &new).unwrap();
    std::fs::rename(&new, &exe).unwrap();
    for (name, args) in [
        ("find_rooms", json!({})),
        ("read_messages", json!({"room_id": "room-1"})),
        (
            "send_message",
            json!({"request_id": "r1", "text": "x", "room_id": "room-1"}),
        ),
    ] {
        let out = m.tool(name, args);
        assert!(out.error, "{}", out.text);
        assert_eq!(out.json()["error"], "identity_unavailable", "{}", out.text);
        assert!(
            out.json()["hint"]
                .as_str()
                .unwrap()
                .contains("Start a new session"),
            "{}",
            out.text
        );
    }
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[test]
fn an_identity_that_cannot_load_is_named_not_internal() {
    let e = signed_in();
    let (base, seen) = stub(api);
    let mut m = mcp_at(&e, &me(), &base);
    std::fs::remove_file(e.state.path().join("identity.key")).unwrap();
    let out = m.tool("find_rooms", json!({}));
    assert!(out.error, "{}", out.text);
    assert_eq!(
        out.json(),
        json!({"error": "identity_unavailable",
            "hint": "This computer is not set up yet. Run lightbulb setup first."}),
        "{}",
        out.text
    );
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn tools_json_lists_remember_and_recall() {
    let tool = |n: &str| spec().iter().find(|t| t["name"] == n).unwrap().clone();
    let (recall, remember) = (tool("recall"), tool("remember"));
    assert_eq!(recall["scope"], "network");
    assert_eq!(remember["scope"], "network");
    assert_eq!(
        recall["description"],
        "Call when a task starts or you need something a teammate may know, not on every turn: the answer is one page of about 8,000 tokens, up to about 9,000 on a full page. It also includes excerpts from your own prompts and replies in this repository on this computer, or in every repository when the folder has no git origin."
    );
    assert_eq!(
        remember["description"],
        "Returns what is already known on the subject; call again with `replaces` if yours is a change. Personal unless the person says to share"
    );
    assert_eq!(recall["inputSchema"]["required"], json!(["question"]));
    let p = &remember["inputSchema"]["properties"];
    assert_eq!(
        p["kind"]["enum"],
        json!(["fact", "decision", "lesson", "preference"])
    );
    assert_eq!(p["scope"]["enum"], json!(["me", "team"]));
    assert_eq!(p["scope"]["default"], "me");
    assert_eq!(p["replaces"]["type"], "array");
    assert_eq!(
        remember["inputSchema"]["required"],
        json!(["subject", "fact", "kind"])
    );
}

#[test]
fn remember_through_mcp_stamps_the_session_and_ignores_undeclared_args() {
    let e = common::Env::new();
    e.daemon(false, &common::computer("mac"), &[]);
    let r = e.ask(
        json!({"id": 1, "op": "register", "session": {"provider": "claude", "conversation": "c1",
        "pid": me().pid, "birth_micros": me().birth_micros, "cwd": "/tmp", "repo": "engine"}}),
    );
    let key = r["key"].as_str().unwrap().to_string();
    let mut m = mcp_on(&e, &me());
    // `when` and `budget_chars` are not in the schema: a model cannot backdate or widen a page.
    let v = m
        .tool(
            "remember",
            json!({"subject": "db", "fact": "postgres 16", "kind": "fact", "when": 1, "budget_chars": 3}),
        )
        .json();
    assert_eq!(v["stored"], true, "{v}");
    assert!(v.get("ok").is_none() && v.get("id").is_none(), "{v}");
    let held = lightbulb::memory::since(&e.store(), &e.identity().key, 0, 10).unwrap();
    let lightbulb::memory::Op::Remember { when, source, .. } = &held[0].op else {
        panic!("{held:?}")
    };
    assert_eq!(*when, None);
    assert_eq!(
        (
            source.harness.as_str(),
            source.repo.as_deref(),
            source.session.as_deref()
        ),
        ("Claude Code", Some("engine"), Some(key.as_str()))
    );
    let page = m
        .tool("recall", json!({"question": "db", "budget_chars": 3}))
        .json();
    assert!(
        page["page"].as_str().unwrap().contains("db: postgres 16"),
        "{page}"
    );
}

/// A question's recall through the tool is the one page: no `block`, no `facts` or `turns`
/// arrays (the daemon's op keeps them for the hook and the benchmark), and a full page stays
/// within 9,000 tokens by the benchmark's count (characters / 4, rounded up).
#[test]
fn a_question_recall_through_mcp_is_one_page() {
    let e = common::Env::new();
    e.daemon(false, &common::computer("mac"), &[]);
    for i in 0..60 {
        let fact = format!(
            "the deploy step {i} runs {}",
            "the vault script again ".repeat(25)
        );
        let r = e.ask(
            json!({"id": 1, "op": "memory.remember", "subject": format!("deploy {i}"),
            "fact": fact, "kind": "fact"}),
        );
        assert_eq!(r["stored"], true, "{r}");
    }
    e.ask(
        json!({"id": 1, "op": "register", "session": {"provider": "claude", "conversation": "c1",
        "pid": me().pid, "birth_micros": me().birth_micros, "cwd": "/tmp", "repo": "engine"}}),
    );
    let mut m = mcp_on(&e, &me());
    let out = m.tool("recall", json!({"question": "deploy vault script"}));
    let v = out.json();
    assert!(v["page"].as_str().unwrap().contains("deploy 0: "), "{v}");
    for k in ["block", "facts", "turns"] {
        assert!(v.get(k).is_none(), "{k} in {v}");
    }
    assert!(v["counts"]["facts"].as_u64().unwrap() > 40, "{v}");
    let tokens = out.text.chars().count().div_ceil(4);
    assert!(tokens <= 9_000, "{tokens} tokens");
    assert!(tokens > 6_000, "a full page: {tokens} tokens");
    // The start view (no question) keeps the daemon's whole answer, `block` included.
    let start = m.tool("recall", json!({"question": ""})).json();
    assert!(
        start.get("block").is_some() && start.get("facts").is_some(),
        "{start}"
    );
}

#[test]
fn the_three_assignment_tools_are_listed_and_assign_round_trips() {
    let net = Net::new();
    let codex = Other::new();
    net.register("claude", "c1", &me());
    let t = net.register("codex", "t1", &codex.1);
    let mut m = net.mcp(&me());

    let listed = m.rpc("tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    for n in ["find_workers", "assign", "cancel_assignment"] {
        assert!(tools.iter().any(|x| x["name"] == n), "{n} not listed");
    }
    let send = tools.iter().find(|x| x["name"] == "send_message").unwrap();
    assert!(!send["inputSchema"]["properties"]["status"].is_null());

    let out = m.tool(
        "assign",
        json!({"request_id": "as1", "to": t, "brief": "run tests", "done_when": "green"}),
    );
    assert!(!out.error, "{}", out.text);
    assert_eq!(out.json()["state"], "stored");
    let r = net.call(json!({"id": 1, "op": "inbox", "key": t}));
    assert_eq!(r["messages"][0]["kind"], "assign", "{r}");

    let r = net.call(json!({"id": 1, "op": "send", "from": t, "reply_to": "as1",
        "request_id": "res1", "text": "green", "kind": "result", "status": "failed",
        "reason": "error"}));
    assert_eq!(r["receipt"]["state"], "stored", "{r}");
    let out = m.tool("check_inbox", json!({"wait_secs": 1}));
    let msgs = out.json()["messages"].clone();
    assert_eq!(msgs.as_array().unwrap().len(), 1, "{msgs}");
    assert_eq!(msgs[0]["kind"], "result");
    assert_eq!(msgs[0]["status"], "failed");
    let out = m.tool("message_receipt", json!({"request_id": "as1"}));
    assert_eq!(out.json()["result"]["status"], "failed", "{}", out.text);

    let out = m.tool(
        "cancel_assignment",
        json!({"assignment": "as1", "request_id": "c1"}),
    );
    assert_eq!(out.json()["state"], "stored", "{}", out.text);
    let r = net.call(json!({"id": 1, "op": "inbox", "key": t}));
    assert_eq!(r["messages"][0]["kind"], "cancel", "{r}");

    let out = m.tool(
        "assign",
        json!({"request_id": "as2", "to": t, "brief": "b", "done_when": "d", "deadline_s": 99999}),
    );
    assert!(out.error);
    assert_eq!(out.json()["error"], "bad_request");
}

#[test]
fn find_workers_lists_always_on_sessions_only() {
    let net = Net::new();
    let codex = Other::new();
    net.register("claude", "c1", &me());
    let t = net.register("codex", "t1", &codex.1);
    let short = net.call(json!({"id": 1, "op": "status"}))["member_key_short"]
        .as_str()
        .unwrap()
        .to_string();
    net.call(json!({"id": 1, "op": "inbox", "key": format!("{short}/friday"), "wait_secs": 0}));
    let mut m = net.mcp(&me());

    // `directory` does not list a name row, so no worker is online here; the registered
    // coding session T is filtered out all the same.
    let v = m.tool("find_workers", json!({})).json();
    let sessions = v["sessions"].as_array().unwrap();
    assert!(sessions.iter().all(|s| s["always_on"] == true), "{v}");
    assert!(
        sessions.iter().all(|s| s["session_id"] != t.as_str()),
        "{v}"
    );
    assert_eq!(v["total"], 0, "{v}");
}
