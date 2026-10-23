//! `mcp` driven over stdio as an agent drives it: the built binary, JSON-RPC lines.
//! The calling agent is pinned with LIGHTBULB_CONNECT_HOOK_CALLER (debug builds only),
//! since the test runner is not an agent.

use lightbulb_connect::{os, paths};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Lines, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

const EXE: &str = env!("CARGO_BIN_EXE_lightbulb-connect");
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
        "startedAt": me().birth_micros / 1000, "status": "idle", "messagingSocketPath": sock});
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
    net.mcp_named(&me(), Some("lightbulb-connect"));
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
    let mut c = e.base_cmd(Command::new(EXE));
    c.arg("mcp").env(
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
    // One team, but bob has not allowed prasiddha: her mail is held on his computer.
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
    assert_eq!(v["state"], "held", "{v}");
    assert_eq!(v["note"], "held: waiting for bob to approve", "{v}");
    let v = m
        .tool("message_receipt", json!({"request_id": "r1"}))
        .json();
    assert_eq!(v["note"], "held: waiting for bob to approve", "{v}");

    // Bob allows prasiddha: the next message is stored on his computer.
    let pa = a.identity().key;
    let r = common::person(
        &b,
        json!({"id": 1, "op": "policy.set", "person_id": pa, "value": "accept"}),
    );
    assert_eq!(r["ok"], true, "{r}");
    let v = m
        .tool(
            "send_message",
            json!({"request_id": "r2", "text": "x", "session_id": to}),
        )
        .json();
    assert_eq!(v["state"], "stored", "{v}");
    assert_eq!(
        v["note"],
        "stored on mbp. It may be held for bob to approve; you will see the receipt change.",
        "{v}"
    );

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

    // The 12-per-10-minutes limit, in words.
    for i in 4..=12 {
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
            json!({"request_id": "r13", "text": "x", "session_id": to}),
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
