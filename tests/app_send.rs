//! A hosted app acting as a person (Claude.ai, ChatGPT, Muse through the hosted MCP) never
//! sends with the workspace member's authority: the member carries the call to one of that
//! person's online computers, which sends it as its own `<short>/<app>-app` session.
//!
//! The world: person P's Macs A ("mac") and A2 ("mac2") and teammate Q's Mac B ("mbp"), all
//! signed into workspace W1; R's Mac C ("mbc") in W2; W1's and W2's members. One roster,
//! seeded in every store before anything starts.

mod common;

use common::{admit, call, endpoint, identity, now_s, person_proof, until, Env, Peer};
use lightbulb_connect::embed::Instance;
use lightbulb_connect::identity::{key_bytes, Identity};
use lightbulb_connect::net::frames::Frame;
use lightbulb_connect::net::ALPN;
use lightbulb_connect::policy::{self, Verdict};
use lightbulb_connect::roster::{self, Entry, Kind};
use lightbulb_connect::store::Store;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

const NAME: &str = "The Lightbulb Company";
const P: &str = "p-prasiddha";
const Q: &str = "p-quinn";
const R: &str = "p-rae";
const NONE_ONLINE: &str = "none of your computers is online right now";

struct World {
    a: Env,
    a2: Env,
    b: Env,
    c: Env,
    w1: tempfile::TempDir,
    w2: Identity,
}

/// Person `pid` (proof name `name`) on `computer`, signed into `ws`.
fn person(who: &Identity, pid: &str, name: &str, computer: &str, ws: &str) -> Value {
    json!({"person_id": pid, "computer": computer,
        "proof": person_proof(who, pid, name, Some(ws))})
}

fn world() -> World {
    let (a, a2, b, c) = (Env::new(), Env::new(), Env::new(), Env::new());
    let w1 = tempfile::tempdir().unwrap();
    let iw1 = Identity::load_or_create(w1.path()).unwrap();
    let iw2 = identity();
    let ia = a.identity();
    let mut gb = json!({"member": ia.key, "name": "prasiddha", "kind": "computer",
        "role": "founder", "ticket_id": "", "team_name": "acme"});
    for (k, v) in person(&ia, P, "Prasiddha", "mac", "W1")
        .as_object()
        .unwrap()
    {
        gb[k] = v.clone();
    }
    let g = Entry::signed(&ia, vec![], Kind::Admit, gb, 1);
    let ia2 = a2.identity();
    let e1 = admit(
        &ia,
        &g,
        &ia2,
        "prasiddha2",
        person(&ia2, P, "Prasiddha", "mac2", "W1"),
    );
    let ib = b.identity();
    let e2 = admit(&ia, &e1, &ib, "quinn", person(&ib, Q, "Quinn", "mbp", "W1"));
    let ic = c.identity();
    let e3 = admit(&ia, &e2, &ic, "rae", person(&ic, R, "Rae", "mbc", "W2"));
    let ws = |who: &Identity, id: &str, name: &str| {
        json!({"kind": "cloud", "person_id": format!("ws:{id}"),
            "proof": common::ws_proof(who, id, name)})
    };
    let e4 = admit(&ia, &e3, &iw1, NAME, ws(&iw1, "W1", NAME));
    let e5 = admit(&ia, &e4, &iw2, "Other Co", ws(&iw2, "W2", "Other Co"));
    let mut stores: Vec<Store> = [&a, &a2, &b, &c].iter().map(|e| e.store()).collect();
    stores.push(Store::open(&w1.path().join("state.db")).unwrap());
    for s in &stores {
        for e in [&g, &e1, &e2, &e3, &e4, &e5] {
            roster::accept(s, e, 10).unwrap();
        }
    }
    // Q takes P's mail; P's computers take Q's replies.
    policy::set_person(&b.store(), P, Verdict::Accept, "local", 1).unwrap();
    for e in [&a, &a2] {
        policy::set_person(&e.store(), Q, Verdict::Accept, "local", 1).unwrap();
    }
    World {
        a,
        a2,
        b,
        c,
        w1,
        w2: iw2,
    }
}

impl World {
    /// Starts B, then each of `envs` dialing B.
    fn run(&self, envs: &[&Env]) {
        self.b.daemon(false, "", &[]);
        for e in envs {
            e.daemon(false, "", &[common::peers(&[&self.b])]);
        }
    }

    /// W1's member, dialing `envs`, once all of them are online.
    async fn instance(&self, envs: &[&Env]) -> Instance {
        let i = common::instance(
            self.w1.path(),
            envs,
            NAME,
            "http://127.0.0.1:9/connect",
            None,
            &[],
        )
        .await
        .unwrap();
        online(&i, envs.len() as u64).await;
        i
    }

    fn q(&self) -> String {
        common::session(&self.b, "q1")
    }

    fn w1_identity(&self) -> Identity {
        Identity::load_or_create(self.w1.path()).unwrap()
    }
}

async fn online(i: &Instance, n: u64) {
    for _ in 0..500 {
        if call(i, "status", json!({})).await["peers"]["online"] == n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{n} online");
}

async fn ok(i: &Instance, op: &str, body: Value) -> Value {
    let r = call(i, op, body).await;
    assert_eq!(r["ok"], true, "{op}: {r}");
    r
}

fn send_body(app: &str, to: &str, rid: &str) -> Value {
    json!({"for_person": P, "app": app, "to": to, "text": format!("hello from {app}"),
        "request_id": rid})
}

fn count(db: &Path, sql: &str) -> i64 {
    let c = rusqlite::Connection::open(db).unwrap();
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn db(e: &Env) -> std::path::PathBuf {
    e.state.path().join("state.db")
}

/// A subscriber connection to `env`'s daemon (reads time out after 10 s).
fn subscribe(env: &Env) -> BufReader<UnixStream> {
    let mut s = UnixStream::connect(env.state.path().join("sock")).unwrap();
    writeln!(s, r#"{{"id":1,"op":"subscribe"}}"#).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line).unwrap(); // the subscribe's answer
    r
}

/// The next event called `name`.
fn event(r: &mut BufReader<UnixStream>, name: &str) -> Value {
    loop {
        let mut line = String::new();
        r.read_line(&mut line).expect("an event within 10 s");
        let v: Value = serde_json::from_str(&line).unwrap();
        if v["event"] == name {
            return v;
        }
    }
}

/// A hand-built `app_*` envelope from `from` to `to`.
fn app_frame(kind: &str, from: &str, to: &str, body: Value) -> Frame {
    let id = common::ulid();
    Frame::Envelope {
        id: id.clone(),
        request_id: id,
        kind: kind.into(),
        from: from.into(),
        to: to.into(),
        reply_to: None,
        thread: None,
        hops: 0,
        body: body.to_string(),
        files: vec![],
        sent_at: now_s(),
    }
}

/// `p`'s next `app_*` envelope, if one comes within 3 s.
async fn app_answer(p: &mut Peer) -> Option<(String, Option<String>, Value)> {
    let got = tokio::time::timeout(
        Duration::from_secs(3),
        p.recv_where(|f| matches!(f, Frame::Envelope { kind, .. } if kind.starts_with("app_"))),
    )
    .await
    .ok()?;
    match got {
        Frame::Envelope {
            kind,
            reply_to,
            body,
            ..
        } => Some((kind, reply_to, serde_json::from_str(&body).unwrap())),
        _ => unreachable!(),
    }
}

/// `who` played by the test, connected to `env`'s daemon.
async fn played(who: &Identity, env: &Env) -> (Peer, iroh::Endpoint) {
    let ep = endpoint(who).await;
    let mut p = Peer::dial(&ep, env).await;
    p.hello(vec![]).await;
    p.presence().await;
    (p, ep)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn app_send_sends_as_the_persons_own_app_session() {
    let w = world();
    w.run(&[&w.a]);
    let i = w.instance(&[&w.a, &w.b]).await;
    let mut events = subscribe(&w.a);
    let q = w.q();
    let r = ok(&i, "app.send", send_body("claude", &q, "r1")).await;
    assert_eq!(
        (
            r["request_id"].as_str(),
            r["state"].as_str(),
            r["computer"].as_str()
        ),
        (Some("r1"), Some("stored"), Some("mac")),
        "{r}"
    );
    assert!(r["thread"].is_string(), "{r}");
    let app = format!("{}/claude-app", w.a.short());
    let e = event(&mut events, "app_sent");
    assert_eq!(
        (e["app"].as_str(), e["to"].as_str()),
        (Some("claude"), Some(q.as_str()))
    );
    assert!(e["id"].is_string(), "{e}");
    let m = w.b.take_one(&q);
    assert_eq!(m["from"], app.as_str());
    assert_eq!(m["label"], "✓ Prasiddha's Claude");
    assert_eq!(m["text"], "hello from claude");
    // Nothing from the workspace member's own sessions reached Q.
    let ws = format!("{}/%", w.w1_identity().short);
    let from_ws = format!("SELECT COUNT(*) FROM messages WHERE from_key LIKE '{ws}'");
    assert_eq!(count(&db(&w.b), &from_ws), 0);
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_computer_online_answers_the_exact_text() {
    let w = world();
    w.run(&[&w.c]);
    let i = w.instance(&[&w.b, &w.c]).await;
    let q = w.q();
    // A and A2 are down; R's C is online but in W2; nobody is no one.
    for who in [P, R, "nobody"] {
        let mut body = send_body("claude", &q, "r1");
        body["for_person"] = who.into();
        let r = call(&i, "app.send", body).await;
        assert_eq!(
            (r["error"].as_str(), r["message"].as_str()),
            (Some("no_computer_online"), Some(NONE_ONLINE)),
            "{who}: {r}"
        );
        for (op, extra) in [
            ("app.inbox", json!({})),
            ("app.receipt", json!({"request_id": "r1"})),
        ] {
            let mut b = json!({"for_person": who, "app": "claude"});
            b.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert_eq!(call(&i, op, b).await["error"], "no_computer_online");
        }
    }
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_computer_is_tried_when_the_first_does_not_answer() {
    let w = world();
    w.run(&[&w.a2]);
    let i = w.instance(&[&w.a2, &w.b]).await;
    // A, played by the test: connected, newest presence, answers nothing.
    let ia = w.a.identity();
    let port = ok(&i, "status", json!({})).await["port"].as_u64().unwrap();
    let key = iroh::PublicKey::from_bytes(&key_bytes(&w.w1_identity().key).unwrap()).unwrap();
    let addr =
        iroh::EndpointAddr::new(key).with_ip_addr(format!("127.0.0.1:{port}").parse().unwrap());
    let ep = endpoint(&ia).await;
    let conn = ep.connect(addr, ALPN).await.unwrap();
    let (send, recv) = conn.open_bi().await.unwrap();
    let mut pa = Peer { conn, send, recv };
    pa.hello(vec![]).await;
    let seq = common::now_s() as u64 * 1000 + 3_600_000;
    pa.send(&common::presence_with(&ia.key, seq, vec![])).await;
    online(&i, 3).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let q = w.q();
    let t = Instant::now();
    let r = ok(&i, "app.send", send_body("claude", &q, "r1")).await;
    assert!(t.elapsed() >= Duration::from_secs(10), "{:?}", t.elapsed());
    assert_eq!(r["computer"], "mac2", "{r}");
    let asked = app_answer(&mut pa).await.expect("A was asked first");
    assert_eq!(asked.0, "app_send");
    let m = w.b.take_one(&q);
    assert_eq!(m["from"], format!("{}/claude-app", w.a2.short()));
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn app_send_from_another_workspaces_member_is_dropped() {
    let w = world();
    w.run(&[&w.a]);
    let (mut p, _ep) = played(&w.w2, &w.a).await;
    let q = w.q();
    let from = format!("{}/claude-app", w.w2.short);
    let to = format!("{}/claude-app", w.a.short());
    p.send(&app_frame(
        "app_send",
        &from,
        &to,
        send_body("claude", &q, "x1"),
    ))
    .await;
    assert_eq!(app_answer(&mut p).await, None);
    assert!(w.b.inbox(&q).is_empty());
    assert!(w.a.status()["dropped"]["app"].as_u64() >= Some(1));
    let mine = format!("SELECT COUNT(*) FROM messages WHERE from_key = '{to}'");
    assert_eq!(count(&db(&w.a), &mine), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn app_send_for_another_person_is_dropped() {
    let w = world();
    w.run(&[&w.a]);
    let iw1 = w.w1_identity();
    let (mut p, _ep) = played(&iw1, &w.a).await;
    let q = w.q();
    let from = format!("{}/claude-app", iw1.short);
    let to = format!("{}/claude-app", w.a.short());
    let mut body = send_body("claude", &q, "x1");
    body["for_person"] = Q.into();
    p.send(&app_frame("app_send", &from, &to, body)).await;
    assert_eq!(app_answer(&mut p).await, None);
    assert!(w.b.inbox(&q).is_empty());
    // The same request for P is served.
    let f = app_frame("app_send", &from, &to, send_body("claude", &q, "x2"));
    let Frame::Envelope { id, .. } = &f else {
        unreachable!()
    };
    let id = id.clone();
    p.send(&f).await;
    let (kind, reply_to, v) = app_answer(&mut p).await.expect("an answer");
    assert_eq!(
        (kind.as_str(), reply_to.as_deref(), v["state"].as_str()),
        ("app_receipt", Some(id.as_str()), Some("stored")),
        "{v}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_app_slug_is_bad_request() {
    let w = world();
    w.run(&[&w.a]);
    let iw1 = w.w1_identity();
    let (mut p, _ep) = played(&iw1, &w.a).await;
    let q = w.q();
    let (from, to) = (
        format!("{}/x-app", iw1.short),
        format!("{}/x-app", w.a.short()),
    );
    for app in ["Claude", "", "a_b", &"x".repeat(25)] {
        p.send(&app_frame("app_send", &from, &to, send_body(app, &q, "x1")))
            .await;
        let (_, _, v) = app_answer(&mut p).await.expect("an answer");
        assert_eq!(v["error"], "bad_request", "{app}: {v}");
    }
    assert!(w.b.inbox(&q).is_empty());
    drop(p);
    // The member refuses it before asking anyone.
    let i = w.instance(&[]).await;
    let r = call(&i, "app.send", send_body("Claude!", &q, "r1")).await;
    assert_eq!(r["error"], "bad_request", "{r}");
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn app_inbox_returns_replies_to_the_app_session_and_marks_read() {
    let w = world();
    w.run(&[&w.a]);
    let i = w.instance(&[&w.a, &w.b]).await;
    let q = w.q();
    ok(&i, "app.send", send_body("claude", &q, "r1")).await;
    let m = w.b.take_one(&q);
    let app = format!("{}/claude-app", w.a.short());
    w.b.send(&q, &app, "hi claude", "q1", m["id"].as_str());
    let inbox = json!({"for_person": P, "app": "claude"});
    let mut records = vec![];
    for _ in 0..250 {
        records = ok(&i, "app.inbox", inbox.clone()).await["records"]
            .as_array()
            .unwrap()
            .clone();
        if !records.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert_eq!(records.len(), 1, "{records:?}");
    let rec = &records[0];
    assert_eq!(
        (
            rec["text"].as_str(),
            rec["from"].as_str(),
            rec["label"].as_str()
        ),
        (Some("hi claude"), Some(q.as_str()), Some("✓ Quinn's Codex"))
    );
    for k in [
        "id",
        "request_id",
        "thread",
        "person",
        "person_verified",
        "computer",
        "sent_at",
    ] {
        assert!(rec.get(k).is_some(), "{k} in {rec}");
    }
    until(10, "Q's reply reads read", || w.b.state(&q, "q1") == "read");
    let again = ok(&i, "app.inbox", inbox).await;
    assert!(again["records"].as_array().unwrap().is_empty(), "{again}");
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn app_receipt_follows_the_sent_message() {
    let w = world();
    w.run(&[&w.a]);
    let i = w.instance(&[&w.a, &w.b]).await;
    let q = w.q();
    ok(&i, "app.send", send_body("claude", &q, "r1")).await;
    let ask = json!({"for_person": P, "app": "claude", "request_id": "r1"});
    let r = ok(&i, "app.receipt", ask.clone()).await;
    assert_eq!(
        (r["state"].as_str(), r["computer"].as_str()),
        (Some("stored"), Some("mac")),
        "{r}"
    );
    w.b.take_one(&q);
    let mut state = String::new();
    for _ in 0..250 {
        state = ok(&i, "app.receipt", ask.clone()).await["state"]
            .as_str()
            .unwrap()
            .into();
        if state == "delivered" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert_eq!(state, "delivered");
    let other = json!({"for_person": P, "app": "chatgpt", "request_id": "r1"});
    assert_eq!(call(&i, "app.receipt", other).await["error"], "not_found");
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn app_kinds_never_enter_a_mailbox_or_hold() {
    let w = world();
    w.run(&[&w.a]);
    let i = w.instance(&[&w.a, &w.b]).await;
    let q = w.q();
    ok(&i, "app.send", send_body("claude", &q, "r1")).await;
    ok(&i, "app.inbox", json!({"for_person": P, "app": "claude"})).await;
    ok(
        &i,
        "app.receipt",
        json!({"for_person": P, "app": "claude", "request_id": "r1"}),
    )
    .await;
    let apps = "SELECT COUNT(*) FROM messages WHERE kind <> 'message'";
    for d in [db(&w.a), db(&w.b), w.w1.path().join("state.db")] {
        assert_eq!(count(&d, apps), 0, "{d:?}");
        assert_eq!(count(&d, "SELECT COUNT(*) FROM holds"), 0, "{d:?}");
    }
    // The member kept nothing: no message, no mailbox row, no outbox.
    let w1 = w.w1.path().join("state.db");
    for t in ["messages", "mailbox", "outbox"] {
        assert_eq!(count(&w1, &format!("SELECT COUNT(*) FROM {t}")), 0, "{t}");
    }
    // A sent one ordinary message.
    assert_eq!(count(&db(&w.a), "SELECT COUNT(*) FROM messages"), 1);
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn app_send_counts_toward_the_persons_limit() {
    let w = world();
    w.run(&[&w.a]);
    let i = w.instance(&[&w.a, &w.b]).await;
    let q = w.q();
    for n in 0..12 {
        let r = ok(&i, "app.send", send_body("claude", &q, &format!("r{n}"))).await;
        assert_eq!(r["state"], "stored", "{n}: {r}");
    }
    let r = ok(&i, "app.send", send_body("claude", &q, "r12")).await;
    assert_eq!(
        (r["state"].as_str(), r["reason"].as_str()),
        (Some("refused"), Some("loop")),
        "{r}"
    );
    i.shutdown().await;
}
