//! The route link: a signed-in computer's daemon dials Lightbulb (a stub WebSocket server on
//! loopback here), proves its key over the nonce and shows its proof, then runs only the app
//! ops for its own person's app sessions; new mail for one sends a content-free notice; a
//! logout closes the link.

mod common;

use common::Env;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio_websockets::{Message, ServerBuilder, WebSocketStream};

const PERSON: &str = "ab12cd34";

struct Link {
    ws: WebSocketStream<TcpStream>,
    next: u64,
}

impl Link {
    async fn recv(&mut self) -> Option<Value> {
        let m = tokio::time::timeout(Duration::from_secs(15), self.ws.next())
            .await
            .expect("a frame within 15 s")?
            .ok()?;
        if m.is_close() {
            return None;
        }
        serde_json::from_str(m.as_text()?).ok()
    }

    async fn send(&mut self, v: Value) {
        self.ws.send(Message::text(v.to_string())).await.unwrap();
    }

    /// One op and its answer (mail notices on the way are kept in `seen`).
    async fn op(&mut self, mut body: Value, seen: &mut Vec<Value>) -> Value {
        self.next += 1;
        body["id"] = self.next.into();
        self.send(body).await;
        loop {
            let f = self.recv().await.expect("an answer");
            if f["id"] == self.next {
                return f;
            }
            seen.push(f);
        }
    }
}

/// A signed-in computer whose daemon dials the stub: the accepted link, after `ready`, and the
/// hello it sent.
async fn linked(e: &Env) -> (Link, Value, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "ws://{}/api/v1/gateway/network",
        listener.local_addr().unwrap()
    );
    e.sign_in("user_aden", Some(&url));
    e.daemon(false, "computer = \"Aden's Mac\"", &[]);
    let (mut link, hello) = accept(&listener).await;
    link.send(json!({"type": "ready", "person": PERSON})).await;
    (link, hello, listener)
}

async fn accept(listener: &TcpListener) -> (Link, Value) {
    let (tcp, _) = tokio::time::timeout(Duration::from_secs(30), listener.accept())
        .await
        .expect("the daemon dials within 30 s")
        .unwrap();
    let (_, ws) = ServerBuilder::new().accept(tcp).await.unwrap();
    let mut link = Link { ws, next: 0 };
    link.send(json!({"type": "challenge", "challenge": "n".repeat(43)}))
        .await;
    let hello = link.recv().await.expect("a hello");
    (link, hello)
}

fn app(op: &str, extra: Value) -> Value {
    let mut v = json!({"op": op, "app": "claude", "person": PERSON, "title": "Claude via Aden"});
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_signed_in_computer_links_and_runs_only_its_persons_app_ops() {
    let e = Env::new();
    let (mut link, hello, _l) = linked(&e).await;
    let id = e.identity();
    // Its key over the nonce, its proof, its name.
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["key"], id.key.as_str());
    let signed = lightbulb::login::proof(&id, "route", &"n".repeat(43), None);
    assert_eq!(hello["sig"], signed["sig"]);
    let stored: Value =
        serde_json::from_slice(&std::fs::read(e.state.path().join("login.json")).unwrap()).unwrap();
    assert_eq!(hello["proof"], stored["proof"]);
    assert_eq!(hello["computer"], "Aden's Mac");

    let mut seen = vec![];
    // Nothing but the app ops, and only for this link's person.
    let r = link.op(json!({"op": "team.leave"}), &mut seen).await;
    assert_eq!(
        (r["ok"].clone(), r["error"].clone()),
        (json!(false), json!("unsupported"))
    );
    let mut other = app("app.unread", json!({}));
    other["person"] = "ffffffff".into();
    let r = link.op(other, &mut seen).await;
    assert_eq!(r["error"], "not_your_person", "{r}");

    // The app session sends (to itself, here), sees its mail, peeks, acks.
    let me = format!("{}/claude-{PERSON}", id.short);
    let sent = link
        .op(
            app(
                "app.send",
                json!({"to": me, "text": "hi", "request_id": "r1"}),
            ),
            &mut seen,
        )
        .await;
    assert_eq!(sent["ok"], true, "{sent}");
    let unread = link.op(app("app.unread", json!({})), &mut seen).await;
    assert_eq!(unread["result"]["unread"], 1, "{unread}");
    let peek = link
        .op(app("app.inbox", json!({"peek": true})), &mut seen)
        .await;
    let records = peek["result"]["records"].as_array().unwrap().clone();
    assert_eq!(records.len(), 1, "{peek}");
    assert_eq!(records[0]["text"], "hi");
    let ids: Vec<Value> = records.iter().map(|r| r["id"].clone()).collect();
    let acked = link
        .op(app("app.ack", json!({ "ids": ids })), &mut seen)
        .await;
    assert_eq!(acked["result"]["acked"], 1, "{acked}");
    let unread = link.op(app("app.unread", json!({})), &mut seen).await;
    assert_eq!(unread["result"]["unread"], 0);
    // The mail notice: the session's name only.
    assert!(
        seen.iter()
            .any(|f| f == &json!({"type": "mail", "session": format!("claude-{PERSON}")})),
        "{seen:?}"
    );
    // The directory lists the app session, titled.
    let dir = link.op(json!({"op": "directory"}), &mut seen).await;
    let sessions = dir["result"]["sessions"].as_array().unwrap();
    assert!(
        sessions
            .iter()
            .any(|s| s["session_id"] == me.as_str() && s["title"] == "Claude via Aden"),
        "{dir}"
    );

    // Logout: the link closes within seconds.
    let out = e.detached(&[common::EXE, "logout"], "");
    assert!(out.status.success(), "{}", common::err(&out));
    let mut closed = false;
    for _ in 0..20 {
        if link.recv().await.is_none() {
            closed = true;
            break;
        }
    }
    assert!(closed, "the link closes after logout");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_computer_that_is_not_signed_in_never_dials() {
    let e = Env::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    e.daemon(false, "", &[]);
    let dialed = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await;
    assert!(dialed.is_err(), "no sign-in, no link");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_only_this_computer_can_use_does_not_leave_it() {
    let e = Env::new();
    let (mut link, _, _l) = linked(&e).await;
    let p = lightbulb::os::process(std::process::id()).unwrap();
    let register = |conversation: &str, extra: Value| {
        let mut session = json!({"provider": "claude", "conversation": conversation,
            "pid": p.pid, "birth_micros": p.birth_micros, "cwd": "/tmp/p"});
        session
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let r = e.ask(json!({"id": 1, "op": "register", "session": session}));
        r["key"].as_str().unwrap().to_string()
    };
    let waits = register(
        "w",
        json!({"status": "waiting", "waiting_for": "permission prompt"}),
    );
    let gone = register("g", json!({}));
    let r = e.ask(json!({"id": 1, "op": "end", "key": gone, "reason": "session-end"}));
    assert_eq!(r["ended"], true, "{r}");

    // A folder turned off after these two registered: they stay on this computer.
    let root = tempfile::tempdir().unwrap();
    let secret = lightbulb::presence::resolved(root.path());
    let cwd = json!({"cwd": secret.to_str().unwrap()});
    let hid = register("h", cwd.clone());
    let hid_gone = register("hg", cwd);
    let r = e.ask(json!({"id": 1, "op": "end", "key": hid_gone, "reason": "session-end"}));
    assert_eq!(r["ended"], true, "{r}");
    let off = lightbulb::presence::resolved(root.path());
    let off = off.to_str().unwrap();
    let off = if cfg!(target_os = "macos") {
        off.to_lowercase()
    } else {
        off.to_string()
    };
    let config = e.state.path().join("config.toml");
    let text = std::fs::read_to_string(&config).unwrap_or_default();
    std::fs::write(&config, format!("{text}\nfolders_off = [{off:?}]\n")).unwrap();
    assert_eq!(e.ask(json!({"id": 1, "op": "reload"}))["ok"], true);

    // Here they are all there; over the link, none, and the rows stay.
    let local = e.ask(json!({"id": 1, "op": "directory"}));
    let row = |v: &Value, key: &str| {
        v.as_array()
            .unwrap()
            .iter()
            .find(|s| s["session_id"] == key)
            .cloned()
            .unwrap_or_else(|| panic!("{key} not in {v}"))
    };
    assert!(row(&local["sessions"], &waits)["pid"].is_u64(), "{local}");
    assert!(
        row(&local["sessions"], &gone)["resume"].is_string(),
        "{local}"
    );
    assert_eq!(row(&local["sessions"], &hid)["hidden"], true, "{local}");
    assert_eq!(
        row(&local["sessions"], &hid_gone)["hidden"],
        true,
        "{local}"
    );
    let mut seen = vec![];
    let dir = link.op(json!({"op": "directory"}), &mut seen).await;
    let sessions = &dir["result"]["sessions"];
    for key in [&hid, &hid_gone] {
        let over = sessions.as_array().unwrap();
        assert!(
            over.iter().all(|s| s["session_id"] != key.as_str()),
            "{dir}"
        );
    }
    let (w, g) = (row(sessions, &waits), row(sessions, &gone));
    assert_eq!(w["status"], "waiting", "{dir}");
    assert_eq!(w["waiting_for"], "permission prompt", "{dir}");
    assert_eq!(g["status"], "ended", "{dir}");
    assert_eq!(g["reason"], "session-end", "{dir}");
    for s in sessions.as_array().unwrap() {
        for f in ["resume", "pid", "birth_micros", "entrypoint", "kind", "job"] {
            assert!(s.get(f).is_none(), "{f}: {s}");
        }
    }
}
