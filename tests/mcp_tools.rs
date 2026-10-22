//! `mcp` driven over stdio as an agent drives it: the built binary, JSON-RPC lines.
//! The calling agent is pinned with LIGHTBULB_NETWORK_HOOK_CALLER (debug builds only),
//! since the test runner is not an agent.

use lightbulb_network::{os, paths};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Lines, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

const EXE: &str = env!("CARGO_BIN_EXE_lightbulb-network");
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
            .env(paths::HOME_ENV, dir.path())
            .env("LIGHTBULB_NETWORK_CLAUDE_HOME", claude.path())
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
        let mut child = Command::new(EXE)
            .arg("mcp")
            .env(paths::HOME_ENV, self.dir.path())
            .env("LIGHTBULB_NETWORK_CLAUDE_HOME", self.claude.path())
            .env(
                "LIGHTBULB_NETWORK_HOOK_CALLER",
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
        };
        let init = m.rpc(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}}),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "lightbulb", "{init}");
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
fn files_or_notify_are_refused_unsupported() {
    let net = Net::new();
    let other = Other::new();
    net.register("claude", "c1", &me());
    let to = net.register("codex", "t1", &other.1);
    let mut m = net.mcp(&me());
    for extra in [json!({"files": []}), json!({"notify": false})] {
        let mut args = json!({"request_id": "r1", "text": "t", "session_id": to});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let out = m.tool("send_message", args);
        assert!(out.error);
        assert_eq!(out.json(), json!({"error": "unsupported"}));
    }
    // Nothing was sent.
    let out = m.tool("message_receipt", json!({"request_id": "r1"}));
    assert!(out.error);
    assert_eq!(out.json(), json!({"error": "not_found"}));
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
    assert_eq!(b["person_verified"], false);
    assert_eq!(b["can_send"], true);
    assert_eq!(b["needs_owner_ok"], false);
    assert_eq!(b["tier"], "pull");
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
