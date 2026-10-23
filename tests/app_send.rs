//! A hosted app acting as a person (Claude.ai, ChatGPT, Muse through the hosted MCP) never
//! sends with the workspace member's authority: the member carries the call to one of that
//! person's online computers, which sends it as its own `<short>/<app>-app` session.
//!
//! The world: person P's Macs A ("mac") and A2 ("mac2") and teammate Q's Mac B ("mbp"), all
//! signed into workspace W1; R's Mac C ("mbc") in W2; W1's and W2's members. One roster,
//! seeded in every store before anything starts.

mod common;

use common::{admit, call, endpoint, identity, now_s, online, person_proof, until, Env, Peer};
use lightbulb_connect::embed::Instance;
use lightbulb_connect::identity::Identity;
use lightbulb_connect::net::frames::{self, Frame};
use lightbulb_connect::policy::{self, Verdict};
use lightbulb_connect::roster::{self, Entry, Kind};
use lightbulb_connect::store::Store;
use serde_json::{json, Value};
use std::collections::HashMap;
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
    world_at(now_s())
}

/// `world`, the workspace members' proofs issued at `iat` (two days back: lapsed).
fn world_at(iat: i64) -> World {
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
            "proof": common::issuer::workspace_proof(&who.key, id, name, &[], iat)})
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
    let (mut pa, _ep) = as_newest_computer(&i, &w.a.identity(), 3).await;
    let q = w.q();
    let t = Instant::now();
    let r = ok(&i, "app.send", send_body("claude", &q, "r1")).await;
    assert!(t.elapsed() >= Duration::from_secs(13), "{:?}", t.elapsed());
    assert_eq!(r["computer"], "mac2", "{r}");
    // Asked to send, then whether it did.
    for kind in ["app_send", "app_receipt_query"] {
        let asked = app_answer(&mut pa).await.expect("A was asked first");
        assert_eq!(asked.0, kind);
    }
    let m = w.b.take_one(&q);
    assert_eq!(m["from"], format!("{}/claude-app", w.a2.short()));
    i.shutdown().await;
}

/// `who` played by the test as a computer of `i`'s, with the newest presence, once `i` has
/// `online` members online.
async fn as_newest_computer(i: &Instance, who: &Identity, online_n: u64) -> (Peer, iroh::Endpoint) {
    let (mut p, ep) = common::dial_instance(i, who).await;
    let seq = now_s() as u64 * 1000 + 3_600_000;
    p.send(&common::presence_with(&who.key, seq, vec![])).await;
    online(i, online_n).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    (p, ep)
}

/// Plays computer `who` (prefix `short`) on `p` in the background: each `app_*` request is
/// answered with `answer(kind, body)` as `(kind, body)`, or not at all.
fn answering(
    mut p: Peer,
    short: String,
    answer: impl Fn(&str, &Value) -> Option<(&'static str, Value)> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok(f) = frames::read(&mut p.recv).await {
            let Frame::Envelope {
                id,
                kind,
                from,
                body,
                ..
            } = f
            else {
                continue;
            };
            let body: Value = serde_json::from_str(&body).unwrap();
            if let Some((k, v)) = answer(&kind, &body) {
                let mut a = app_frame(k, &format!("{short}/claude-app"), &from, v);
                if let Frame::Envelope { reply_to, .. } = &mut a {
                    *reply_to = Some(id);
                }
                frames::write(&mut p.send, &a).await.unwrap();
            }
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_silent_computer_that_sent_is_not_resent() {
    let w = world();
    w.run(&[&w.a2]);
    let i = w.instance(&[&w.a2, &w.b]).await;
    let (pa, _ep) = as_newest_computer(&i, &w.a.identity(), 3).await;
    // A sent (its answer was lost): asked again, it has the receipt.
    let _a = answering(pa, w.a.short(), |kind, b| {
        (kind == "app_receipt_query").then(|| {
            (
                "app_receipt",
                json!({"app": "claude", "request_id": b["request_id"],
                "id": common::ulid(), "state": "stored", "thread": common::ulid()}),
            )
        })
    });
    let r = ok(&i, "app.send", send_body("claude", &w.q(), "r1")).await;
    assert_eq!(
        (r["computer"].as_str(), r["state"].as_str()),
        (Some("mac"), Some("stored")),
        "{r}"
    );
    assert!(w.b.inbox(&w.q()).is_empty(), "A2 sent it again");
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbox_skips_a_computer_that_answers_an_error() {
    let w = world();
    w.run(&[&w.a2]);
    let i = w.instance(&[&w.a2, &w.b]).await;
    let q = w.q();
    let to_a2 = format!("{}/claude-app", w.a2.short());
    w.b.send(&q, &to_a2, "for a2", "q1", None);
    until(10, "stored on A2", || w.b.state(&q, "q1") == "stored");
    let (pa, _ep) = as_newest_computer(&i, &w.a.identity(), 3).await;
    // A answers every inbox with an error, a send with an unknown code, and the wrong kind
    // to a receipt query.
    let _a = answering(pa, w.a.short(), |kind, b| match kind {
        "app_inbox" => Some((
            "app_inbox_result",
            json!({"app": "claude", "error": "bad_request"}),
        )),
        "app_send" => Some((
            "app_receipt",
            json!({"app": "claude",
            "request_id": b["request_id"], "error": "weird\ncode"}),
        )),
        _ => Some(("app_inbox_result", json!({"app": "claude", "records": []}))),
    });
    let inbox = ok(&i, "app.inbox", json!({"for_person": P, "app": "claude"})).await;
    let recs = inbox["records"].as_array().unwrap();
    assert_eq!(recs.len(), 1, "{inbox}");
    assert_eq!(
        (recs[0]["text"].as_str(), inbox["more"].as_bool()),
        (Some("for a2"), Some(false))
    );
    let r = call(&i, "app.send", send_body("claude", &q, "r1")).await;
    assert_eq!(r["error"], "internal", "{r}");
    // A wrong-kind answer is no answer: A2 says it has none of it.
    let ask = json!({"for_person": P, "app": "claude", "request_id": "r9"});
    assert_eq!(call(&i, "app.receipt", ask).await["error"], "not_found");
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbox_answers_are_bounded_and_marked_read_after_the_write() {
    let w = world();
    w.run(&[&w.a]);
    let i = w.instance(&[&w.a, &w.b]).await;
    let q = w.q();
    let app = format!("{}/claude-app", w.a.short());
    let big = "x".repeat(60_000);
    for n in 0..12 {
        let text = format!("{n:02}{big}");
        w.b.send(&q, &app, &text, &format!("q{n}"), None);
    }
    until(10, "all stored on A", || {
        (0..12).all(|n| w.b.state(&q, &format!("q{n}")) == "stored")
    });
    let inbox = json!({"for_person": P, "app": "claude"});
    let mut got = vec![];
    let mut rounds = 0;
    loop {
        let r = ok(&i, "app.inbox", inbox.clone()).await;
        let recs = r["records"].as_array().unwrap();
        assert!(
            !recs.is_empty() && recs.len() < 12,
            "{} records",
            recs.len()
        );
        got.extend(
            recs.iter()
                .map(|m| m["text"].as_str().unwrap()[..2].to_string()),
        );
        rounds += 1;
        if r["more"] == false {
            break;
        }
        assert!(rounds < 12);
    }
    got.sort();
    let want: Vec<String> = (0..12).map(|n| format!("{n:02}")).collect();
    assert_eq!(got, want);
    // Each read once it went out, and A stayed connected throughout.
    until(10, "all read", || {
        (0..12).all(|n| w.b.state(&q, &format!("q{n}")) == "read")
    });
    assert_eq!(ok(&i, "status", json!({})).await["peers"]["online"], 2);
    let r = ok(&i, "app.inbox", inbox).await;
    assert!(r["records"].as_array().unwrap().is_empty(), "{r}");
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lapsed_workspace_member_has_no_authority() {
    let w = world_at(now_s() - 2 * 86_400);
    w.run(&[&w.a]);
    let iw1 = w.w1_identity();
    let (mut p, _ep) = played(&iw1, &w.a).await;
    let from = format!("{}/claude-app", iw1.short);
    let to = format!("{}/claude-app", w.a.short());
    p.send(&app_frame(
        "app_send",
        &from,
        &to,
        send_body("claude", &w.q(), "x1"),
    ))
    .await;
    assert_eq!(app_answer(&mut p).await, None);
    assert!(w.b.inbox(&w.q()).is_empty());
    let g = common::person(
        &w.a,
        json!({"id": 1, "op": "share.grant", "session": common::session(&w.a, "a1"),
            "viewer": iw1.key, "input": true, "expires_at": now_s() + 600}),
    );
    assert_eq!(g["error"], "unsupported", "{g}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn app_envelope_from_outside_the_members_prefix_is_dropped() {
    let w = world();
    w.run(&[&w.a]);
    let iw1 = w.w1_identity();
    let (mut p, _ep) = played(&iw1, &w.a).await;
    let to = format!("{}/claude-app", w.a.short());
    for from in [format!("{}/claude-app", w.b.short()), "nobody".to_string()] {
        p.send(&app_frame(
            "app_send",
            &from,
            &to,
            send_body("claude", &w.q(), "x1"),
        ))
        .await;
        assert_eq!(app_answer(&mut p).await, None, "{from}");
    }
    assert!(w.a.status()["dropped"]["app"].as_u64() >= Some(2));
    assert!(w.b.inbox(&w.q()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_teammate_who_has_not_allowed_p_holds_ps_claude() {
    let w = world();
    policy::set_person(&w.b.store(), P, Verdict::Hold, "local", 2).unwrap();
    w.run(&[&w.a]);
    let i = w.instance(&[&w.a, &w.b]).await;
    let r = ok(&i, "app.send", send_body("claude", &w.q(), "r1")).await;
    assert_eq!(r["state"], "held", "{r}");
    assert_eq!(count(&db(&w.b), "SELECT COUNT(*) FROM holds"), 1);
    assert!(w.b.inbox(&w.q()).is_empty());
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_limit_is_the_persons_across_both_computers() {
    let w = world();
    w.run(&[&w.a, &w.a2]);
    let i = w.instance(&[&w.a, &w.a2, &w.b]).await;
    let q = w.q();
    let mut by: HashMap<String, usize> = HashMap::new();
    for n in 0..12 {
        let r = ok(&i, "app.send", send_body("claude", &q, &format!("r{n}"))).await;
        assert_eq!(r["state"], "stored", "{n}: {r}");
        *by.entry(r["computer"].as_str().unwrap().into())
            .or_default() += 1;
    }
    // A computer that sent all twelve is stopped: the 13th goes through one under its own
    // count, and Q's computer counts the person.
    if let Some((full, _)) = by.iter().find(|(_, n)| **n == 12) {
        let e = if full == "mac" { &w.a } else { &w.a2 };
        e.ask(json!({"id": 1, "op": "shutdown"}));
        online(&i, 2).await;
    }
    let r = ok(&i, "app.send", send_body("claude", &q, "r12")).await;
    assert_eq!(
        (
            r["state"].as_str(),
            r["reason"].as_str(),
            r["limit"].as_str()
        ),
        (Some("refused"), Some("loop"), Some("rate")),
        "{r}"
    );
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
