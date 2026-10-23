//! Mail between computers: the sender's outbox, the wire envelope, the recipient's checks,
//! acks and later receipts, and `leave` told to a peer. Each test runs real daemons; a test
//! that needs to hand-build frames plays one member itself over its own endpoint (`Peer`).

use lightbulb_network::identity::Identity;
use lightbulb_network::net::frames::Frame;
use lightbulb_network::roster::{self, Entry, Kind};
use serde_json::{json, Value};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod common;

use common::{endpoint, err, identity, out, peers, person, remote, seed_pair, sees, until};
use common::{Env, Peer, EXE};

fn short(e: &Env) -> String {
    e.identity().short
}

/// `<short>/codex:<conv>` on `e`.
fn session(e: &Env, conv: &str) -> String {
    format!("{}/codex:{conv}", short(e))
}

fn send(e: &Env, from: &str, to: &str, text: &str, rid: &str, reply_to: Option<&str>) -> Value {
    let r = e.ask(
        json!({"id": 1, "op": "send", "from": from, "to": to, "text": text,
        "request_id": rid, "reply_to": reply_to}),
    );
    assert_eq!(r["ok"], true, "{r}");
    r["receipt"].clone()
}

fn receipt(e: &Env, from: &str, rid: &str) -> Value {
    e.ask(json!({"id": 1, "op": "receipt", "from": from, "request_id": rid}))["receipt"].clone()
}

fn state(e: &Env, from: &str, rid: &str) -> String {
    receipt(e, from, rid)["state"]
        .as_str()
        .unwrap_or_default()
        .into()
}

fn inbox(e: &Env, key: &str) -> Vec<Value> {
    let r = e.ask(json!({"id": 1, "op": "inbox", "key": key}));
    r["messages"].as_array().cloned().unwrap_or_default()
}

/// Waits for one message to `key` on `e` and takes it.
fn take_one(e: &Env, key: &str) -> Value {
    let mut got = vec![];
    until(10, "a message arrives", || {
        got = inbox(e, key);
        !got.is_empty()
    });
    assert_eq!(got.len(), 1, "{got:?}");
    got.remove(0)
}

fn unread(e: &Env, key: &str) -> u64 {
    e.ask(json!({"id": 1, "op": "unread", "key": key}))["unread"]
        .as_u64()
        .unwrap()
}

fn wait_state(e: &Env, from: &str, rid: &str, want: &str) {
    until(10, &format!("{rid} reads {want}"), || {
        state(e, from, rid) == want
    });
}

/// A and B in one team, both running, A dialing B (`more` set on A).
fn pair(more: &[(&str, String)]) -> (Env, Env) {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    b.daemon(false, "", &[]);
    let mut env = vec![peers(&[&b])];
    env.extend_from_slice(more);
    a.daemon(false, "", &env);
    let one = json!({"online": 1, "total": 1});
    until(10, "both online", || {
        a.status()["peers"] == one && b.status()["peers"] == one
    });
    (a, b)
}

/// `who` admitted into `e`'s stored team as "carol" (computer "cmac"), a computer or (with
/// `cloud`) a cloud member with an opaque proof, signed by the founder `founder`.
fn admit(e: &Env, founder: &Identity, parent: &Entry, who: &Identity, cloud: bool) -> Entry {
    let mut body = json!({"member": who.key, "name": "carol", "computer": "cmac",
        "person_id": who.key, "kind": "computer", "role": "member"});
    if cloud {
        body["kind"] = json!("cloud");
        body["proof"] = json!("p");
    }
    let entry = Entry::signed(founder, vec![parent.hash()], Kind::Admit, body, 3);
    assert!(roster::accept(&e.store(), &entry, 10).unwrap());
    entry
}

fn ulid() -> String {
    ulid::Ulid::generate().to_string()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn envelope(id: &str, rid: &str, from: &str, to: &str, body: &str) -> Frame {
    Frame::Envelope {
        id: id.into(),
        request_id: rid.into(),
        kind: "message".into(),
        from: from.into(),
        to: to.into(),
        reply_to: None,
        thread: None,
        hops: 0,
        body: body.into(),
        files: vec![],
        sent_at: 0,
    }
}

impl Peer {
    /// The `ack` for envelope `id`, as `(state, reason)`.
    async fn ack(&mut self, id: &str) -> (String, Option<String>) {
        match self
            .recv_where(|f| matches!(f, Frame::Ack { id: i, .. } if i == id))
            .await
        {
            Frame::Ack { state, reason, .. } => (state, reason),
            _ => unreachable!(),
        }
    }

    /// The next frame that is an ack (of any envelope): its id.
    async fn any_ack(&mut self) -> String {
        match self.recv_where(|f| matches!(f, Frame::Ack { .. })).await {
            Frame::Ack { id, .. } => id,
            _ => unreachable!(),
        }
    }

    /// The next envelope: its id, thread and hops.
    async fn envelope(&mut self) -> (String, Option<String>, u32) {
        match self
            .recv_where(|f| matches!(f, Frame::Envelope { .. }))
            .await
        {
            Frame::Envelope {
                id, thread, hops, ..
            } => (id, thread, hops),
            _ => unreachable!(),
        }
    }

    async fn answer(&mut self, ack: bool, id: &str, state: &str) {
        let (id, state, reason) = (id.to_string(), state.to_string(), None);
        let f = if ack {
            Frame::Ack { id, state, reason }
        } else {
            Frame::Receipt { id, state, reason }
        };
        self.send(&f).await;
    }
}

/// A running alone, B played by the test (connected, past its `hello`).
async fn a_and_peer_b() -> (Env, Env, iroh::Endpoint, Peer) {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    a.daemon(false, "", &[]);
    let ep = endpoint(&b.identity()).await;
    let mut p = Peer::dial(&ep, &a).await;
    p.hello(vec![]).await;
    until(5, "B online", || a.status()["peers"]["online"] == 1);
    (a, b, ep, p)
}

/// Stops `e`'s daemon and waits until its socket is gone.
fn stop(e: &Env) {
    e.ask(json!({"id": 1, "op": "shutdown"}));
    until(10, "the daemon stops", || {
        std::os::unix::net::UnixStream::connect(e.state.path().join("sock")).is_err()
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_to_teammate_walks_queued_stored_delivered_read() {
    let (a, b) = pair(&[]);
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let r = send(&a, &sa, &sb, "hello bob", "r1", None);
    assert_eq!(
        (r["state"].as_str(), r["where"].as_str()),
        (Some("queued"), Some("mbp")),
        "{r}"
    );
    wait_state(&a, &sa, "r1", "stored");
    let m = take_one(&b, &sb);
    assert_eq!(
        (m["from"].as_str(), m["text"].as_str()),
        (Some(sa.as_str()), Some("hello bob"))
    );
    assert!(
        m["framed"]
            .as_str()
            .unwrap()
            .contains("from prasiddha's Codex (not verified)"),
        "{m}"
    );
    wait_state(&a, &sa, "r1", "delivered");
    until(10, "read", || {
        b.ask(json!({"id": 1, "op": "mark_read", "key": sb}));
        state(&a, &sa, "r1") == "read"
    });
    // A retry of the same request is the same receipt, still on the teammate's computer.
    let again = send(&a, &sa, &sb, "hello bob", "r1", None);
    assert_eq!(
        (again["state"].as_str(), again["where"].as_str()),
        (Some("read"), Some("mbp"))
    );
    let r = send(&a, &sa, &sb, "other text", "r1", None);
    assert_eq!(r["reason"], "request_id_reused");
    // A prefix that is no member's.
    let r = a.ask(
        json!({"id": 1, "op": "send", "from": sa, "to": "zzzzzzzzzzzz/codex:x",
        "text": "x", "request_id": "r2"}),
    );
    assert_eq!(r["error"], "not_member", "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_recipient_stays_queued_then_stores_on_reconnect() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    a.daemon(false, "", &[]);
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    assert_eq!(send(&a, &sa, &sb, "later", "r1", None)["state"], "queued");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(state(&a, &sa, "r1"), "queued");
    // B comes up and dials A: the outbox flushes on its hello.
    b.daemon(false, "", &[peers(&[&a])]);
    wait_state(&a, &sa, "r1", "stored");
    assert_eq!(take_one(&b, &sb)["text"], "later");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outbox_survives_restart_and_dedupes() {
    let drop_acks = ("LIGHTBULB_NETWORK_DEBUG_DROP_ACKS", "1".to_string());
    let (a, b) = pair(&[drop_acks]);
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    send(&a, &sa, &sb, "once only", "r1", None);
    until(10, "B stores it", || unread(&b, &sb) == 1);
    // A never hears the ack, so it sends again; B answers from what it has seen.
    let line = format!("from {}: stored", short(&a));
    until(15, "B acks a duplicate", || {
        let log = std::fs::read_to_string(b.state.path().join("daemon.log")).unwrap();
        log.matches(&line).count() >= 2
    });
    assert_eq!(state(&a, &sa, "r1"), "queued");
    stop(&a);
    a.daemon(false, "", &[peers(&[&b])]);
    wait_state(&a, &sa, "r1", "stored");
    assert_eq!(unread(&b, &sb), 1);
    assert_eq!(take_one(&b, &sb)["text"], "once only");
    wait_state(&a, &sa, "r1", "delivered");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outbox_expires_after_24h_unreachable() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    a.daemon(
        false,
        "",
        &[("LIGHTBULB_NETWORK_DEBUG_OUTBOX_S", "2".into())],
    );
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    assert_eq!(send(&a, &sa, &sb, "x", "r1", None)["state"], "queued");
    wait_state(&a, &sa, "r1", "expired");
    let r = receipt(&a, &sa, "r1");
    assert_eq!(r["reason"], "unreachable", "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_duplicate_ack_never_regresses_a_receipt() {
    // The recipient's side: a resent envelope is re-acked with its newest state.
    let (b, a) = (Env::new(), Env::new());
    seed_pair(&b, &a);
    b.daemon(false, "", &[]);
    let ep = endpoint(&a.identity()).await;
    let mut p = Peer::dial(&ep, &b).await;
    p.hello(vec![]).await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let f = envelope(&id, "r1", &sa, &sb, "hi");
    p.send(&f).await;
    assert_eq!(p.ack(&id).await.0, "stored"); // and the test, as A, "drops" it
    take_one(&b, &sb);
    until(10, "B reads it", || {
        b.ask(json!({"id": 1, "op": "mark_read", "key": sb}));
        state(&b, &sa, "r1") == "read"
    });
    p.recv_where(
        |f| matches!(f, Frame::Receipt { id: i, state, .. } if *i == id && state == "read"),
    )
    .await;
    p.send(&f).await;
    assert_eq!(p.ack(&id).await, ("read".into(), None));
    assert_eq!(unread(&b, &sb), 0, "delivered once");

    // The sender's side: a late `stored` cannot pull a `read` back.
    drop(p);
    let (a, b2, _ep, mut p) = a_and_peer_b().await;
    let (sa, sb) = (session(&a, "a1"), session(&b2, "b1"));
    send(&a, &sa, &sb, "hi", "r1", None);
    let (id, ..) = p.envelope().await;
    p.answer(false, &id, "read").await;
    wait_state(&a, &sa, "r1", "read");
    p.answer(true, &id, "stored").await;
    // A frame after it, seen processed: the presence of a session.
    let s = session(&b2, "seen");
    p.send(&common::presence_with(
        &b2.identity().key,
        now_ms(),
        vec![remote(&s)],
    ))
    .await;
    until(5, "the presence after the late ack", || sees(&a, &s));
    assert_eq!(state(&a, &sa, "r1"), "read");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_receipt_advances_to_stored_on_allow() {
    let (a, b, _ep, mut p) = a_and_peer_b().await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    send(&a, &sa, &sb, "may I", "r1", None);
    let (id, ..) = p.envelope().await;
    p.answer(true, &id, "held").await;
    wait_state(&a, &sa, "r1", "held");
    p.answer(false, &id, "stored").await;
    wait_state(&a, &sa, "r1", "stored");
    p.answer(false, &id, "delivered").await;
    wait_state(&a, &sa, "r1", "delivered");
    p.answer(true, &id, "held").await;
    let s = session(&b, "seen");
    p.send(&common::presence_with(
        &b.identity().key,
        now_ms(),
        vec![remote(&s)],
    ))
    .await;
    until(5, "the presence after the late ack", || sees(&a, &s));
    assert_eq!(state(&a, &sa, "r1"), "delivered");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn envelope_from_another_members_prefix_is_dropped() {
    let (a, b, _ep, mut p) = a_and_peer_b().await;
    let to = session(&a, "a1");
    // From A's own prefix, and from no member's: dropped without an ack.
    for from in [session(&a, "x"), "zzzzzzzzzzzz/codex:x".to_string()] {
        p.send(&envelope(&ulid(), "r1", &from, &to, "forged")).await;
    }
    let good = ulid();
    p.send(&envelope(&good, "r2", &session(&b, "b1"), &to, "real"))
        .await;
    assert_eq!(p.any_ack().await, good);
    assert_eq!(unread(&a, &to), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cloud_envelope_from_an_unadvertised_session_is_refused() {
    let (a, b) = (Env::new(), Env::new());
    let [_, admitted] = seed_pair(&a, &b);
    let c = identity();
    admit(&a, &a.identity(), &admitted, &c, true);
    a.daemon(false, "", &[]);
    let ep = endpoint(&c).await;
    let mut p = Peer::dial(&ep, &a).await;
    p.hello(vec![]).await;
    let (friday, atlas) = (format!("{}/friday", c.short), format!("{}/atlas", c.short));
    p.send(&Frame::Presence {
        member: c.key.clone(),
        seq: now_ms(),
        sessions: vec![],
        known: vec![friday.clone(), atlas.clone()],
        kept: vec![],
    })
    .await;
    let to = session(&a, "a1");
    let id = ulid();
    p.send(&envelope(&id, "r1", &format!("{friday}-2"), &to, "hi"))
        .await;
    assert_eq!(p.ack(&id).await, ("refused".into(), Some("policy".into())));
    let id = ulid();
    p.send(&envelope(&id, "r2", &friday, &to, "hi")).await;
    assert_eq!(p.ack(&id).await.0, "stored");
    assert_eq!(take_one(&a, &to)["from"], friday.as_str());
    // A cloud member's principal is the sending session: friday's 13th in ten minutes is a
    // loop, atlas still gets through.
    for i in 1..13 {
        let id = ulid();
        p.send(&envelope(&id, &format!("f{i}"), &friday, &to, "x"))
            .await;
        let want = if i < 12 { "stored" } else { "refused" };
        assert_eq!(p.ack(&id).await.0, want, "send {i}");
    }
    let id = ulid();
    p.send(&envelope(&id, "a1", &atlas, &to, "x")).await;
    assert_eq!(p.ack(&id).await.0, "stored");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ack_from_a_third_member_is_ignored() {
    let (a, b) = (Env::new(), Env::new());
    let [_, admitted] = seed_pair(&a, &b);
    let c = identity();
    admit(&a, &a.identity(), &admitted, &c, false);
    a.daemon(false, "", &[]);
    let (ep_b, ep_c) = (endpoint(&b.identity()).await, endpoint(&c).await);
    let mut pb = Peer::dial(&ep_b, &a).await;
    pb.hello(vec![]).await;
    let mut pc = Peer::dial(&ep_c, &a).await;
    pc.hello(vec![]).await;
    until(5, "both online", || a.status()["peers"]["online"] == 2);
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    send(&a, &sa, &sb, "for bob", "r1", None);
    let (id, ..) = pb.envelope().await;
    pc.answer(true, &id, "read").await;
    pc.answer(false, &id, "read").await;
    let s = format!("{}/codex:seen", c.short);
    pc.send(&common::presence_with(&c.key, now_ms(), vec![remote(&s)]))
        .await;
    until(5, "the presence after the forged acks", || sees(&a, &s));
    assert_eq!(state(&a, &sa, "r1"), "queued");
    pb.answer(true, &id, "stored").await;
    wait_state(&a, &sa, "r1", "stored");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fourth_hop_and_thirteenth_send_are_refused_loop() {
    let (a, b) = (Env::new(), Env::new());
    let [_, admitted] = seed_pair(&a, &b);
    let c = identity();
    admit(&a, &a.identity(), &admitted, &c, false);
    admit(&b, &a.identity(), &admitted, &c, false);
    b.daemon(false, "", &[]);
    a.daemon(false, "", &[peers(&[&b])]);
    until(10, "A and B online", || a.status()["peers"]["online"] == 1);

    // With reply_to: hops 0, 1, 2, 3 go; the fourth agent reply is refused.
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    send(&a, &sa, &sb, "h0", "h0", None);
    let ends = [(&b, &sb, &sa), (&a, &sa, &sb)];
    let mut first = String::new(); // A's first message, by id
    for hop in 1..=4 {
        let (me, at, there) = ends[(hop - 1) % 2];
        let m = take_one(me, at);
        assert_eq!(m["hops"], hop - 1);
        if hop == 1 {
            first = m["id"].as_str().unwrap().into();
        }
        let rid = format!("h{hop}");
        let r = send(
            me,
            at,
            there,
            "re",
            &rid,
            Some(m["request_id"].as_str().unwrap()),
        );
        if hop < 4 {
            wait_state(me, at, &rid, "stored");
        } else {
            assert_eq!(
                (r["state"].as_str(), r["reason"].as_str()),
                (Some("refused"), Some("loop"))
            );
        }
    }

    // A forward chain without reply_to, through new sessions each time.
    let chain: Vec<(&Env, String)> = (0..6)
        .map(|i| {
            let e = if i % 2 == 0 { &a } else { &b };
            (e, session(e, &format!("f{i}")))
        })
        .collect();
    send(chain[0].0, &chain[0].1, &chain[1].1, "go", "f0", None);
    for i in 1..=4 {
        let (me, at) = (chain[i].0, &chain[i].1);
        assert_eq!(take_one(me, at)["hops"], i - 1);
        let rid = format!("f{i}");
        let r = send(me, at, &chain[i + 1].1, "pass it on", &rid, None);
        if i < 4 {
            wait_state(me, at, &rid, "stored");
        } else {
            assert_eq!(r["reason"], "loop", "{r}");
        }
    }

    // Thirteen sends in ten minutes from one session to one session.
    let (s1, s2) = (session(&a, "t1"), session(&b, "t2"));
    for i in 0..12 {
        assert_eq!(
            send(&a, &s1, &s2, "x", &format!("t{i}"), None)["state"],
            "queued"
        );
    }
    assert_eq!(send(&a, &s1, &s2, "x", "t12", None)["reason"], "loop");

    // The recipient's own checks, against a member the test plays: hops 4, and the
    // thirteenth from one person, whatever session it comes from.
    let ep = endpoint(&c).await;
    let mut p = Peer::dial(&ep, &a).await;
    p.hello(vec![]).await;
    let to = session(&a, "inbox");
    let id = ulid();
    let mut f = envelope(&id, "deep", &format!("{}/codex:d", c.short), &to, "x");
    if let Frame::Envelope { hops, .. } = &mut f {
        *hops = 4;
    }
    p.send(&f).await;
    assert_eq!(p.ack(&id).await, ("refused".into(), Some("loop".into())));
    // An answer to (or in the thread of) A's own message is at least one hop past it.
    for by_thread in [false, true] {
        let id = ulid();
        let mut f = envelope(&id, "low", &format!("{}/codex:d", c.short), &to, "x");
        if let Frame::Envelope {
            reply_to, thread, ..
        } = &mut f
        {
            *if by_thread { thread } else { reply_to } = Some(first.clone());
        }
        p.send(&f).await;
        assert_eq!(p.ack(&id).await, ("refused".into(), Some("loop".into())));
    }
    for i in 0..13 {
        let id = ulid();
        let from = format!("{}/codex:s{i}", c.short);
        p.send(&envelope(&id, &format!("p{i}"), &from, &to, "x"))
            .await;
        let want = if i < 12 {
            ("stored".into(), None)
        } else {
            ("refused".into(), Some("loop".into()))
        };
        assert_eq!(p.ack(&id).await, want, "send {i}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_person_at_the_terminal_resets_hops_to_zero() {
    // The test process counts as an agent on A (debug pin): only a person speaks as the terminal.
    let birth = lightbulb_network::os::process(std::process::id())
        .unwrap()
        .birth_micros;
    let pin = format!("{}:{birth}", std::process::id());
    let (a, b) = pair(&[("LIGHTBULB_NETWORK_HOOK_CALLER", pin)]);
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    send(&a, &sa, &sb, "q", "r0", None);
    let m = take_one(&b, &sb);
    send(
        &b,
        &sb,
        &sa,
        "a",
        "r1",
        Some(m["request_id"].as_str().unwrap()),
    );
    let m = take_one(&a, &sa);
    assert_eq!(m["hops"], 1);
    // The person answers in that thread: hop 0 again.
    let terminal = format!("{}/terminal", short(&a));
    let req = json!({"id": 1, "op": "send", "from": terminal, "to": sb, "text": "go on",
        "request_id": "r2", "reply_to": m["id"]});
    let r = a.ask(req.clone());
    assert_eq!(
        (r["error"].as_str(), r["reason"].as_str()),
        (Some("bad_request"), Some("not_a_person")),
        "{r}"
    );
    let r = person(&a, req);
    assert_eq!(r["receipt"]["state"], "queued", "{r}");
    let m2 = take_one(&b, &sb);
    assert_eq!(
        (m2["hops"].as_i64(), &m2["thread"]),
        (Some(0), &m["thread"])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wire_refusals_name_their_reason() {
    let (a, b, _ep, mut p) = a_and_peer_b().await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    p.send(&envelope(&id, "r1", &sb, &sa, "one")).await;
    assert_eq!(p.ack(&id).await.0, "stored");
    let refused = |r: &str| ("refused".to_string(), Some(r.to_string()));
    for (to, text, thread, reason) in [
        (sa.as_str(), "two", None, "request_id_reused"),
        ("*", "one", None, "policy"),
        (&format!("{}/*", short(&a)), "one", None, "policy"),
        (sb.as_str(), "one", None, "not_member"),
        (sa.as_str(), "one", Some("not-a-ulid"), "unsupported"),
    ] {
        let id = ulid();
        let mut f = envelope(&id, "r1", &sb, to, text);
        if let Frame::Envelope { thread: t, .. } = &mut f {
            *t = thread.map(String::from);
        }
        p.send(&f).await;
        assert_eq!(p.ack(&id).await, refused(reason), "{to} {text}");
    }
    assert_eq!(unread(&a, &sa), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_kind_is_refused_unsupported() {
    let (a, b, _ep, mut p) = a_and_peer_b().await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let r = a.ask(
        json!({"id": 1, "op": "send", "from": sa, "to": sb, "text": "ls",
        "request_id": "r1", "kind": "input"}),
    );
    assert_eq!(r["error"], "unsupported", "{r}");
    let id = ulid();
    let mut f = envelope(&id, "r1", &sb, &sa, "ls");
    if let Frame::Envelope { kind, .. } = &mut f {
        *kind = "input".into();
    }
    p.send(&f).await;
    assert_eq!(
        p.ack(&id).await,
        ("refused".into(), Some("unsupported".into()))
    );
    assert_eq!(unread(&a, &sa), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_or_oversize_body_are_refused_unsupported() {
    let (a, b, _ep, mut p) = a_and_peer_b().await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let mut f = envelope(&id, "r1", &sb, &sa, "see attached");
    if let Frame::Envelope { files, .. } = &mut f {
        *files = vec!["x".into()];
    }
    p.send(&f).await;
    assert_eq!(
        p.ack(&id).await,
        ("refused".into(), Some("unsupported".into()))
    );
    let id = ulid();
    p.send(&envelope(&id, "r2", &sb, &sa, &"x".repeat(65_537)))
        .await;
    assert_eq!(
        p.ack(&id).await,
        ("refused".into(), Some("unsupported".into()))
    );
    let id = ulid();
    p.send(&envelope(&id, "r3", &sb, &sa, &"x".repeat(65_536)))
        .await;
    assert_eq!(p.ack(&id).await.0, "stored");
    assert_eq!(unread(&a, &sa), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn implicit_reply_keeps_the_trigger_thread() {
    let (a, b) = pair(&[]);
    let (sa, sb, sc) = (session(&a, "a1"), session(&b, "b1"), session(&a, "c1"));
    send(&a, &sa, &sb, "q", "r0", None);
    let m = take_one(&b, &sb);
    let thread = m["thread"].clone();
    assert_eq!((m["hops"].as_i64(), &m["id"]), (Some(0), &thread));
    send(&b, &sb, &sa, "answer, no reply_to", "r1", None);
    let back = take_one(&a, &sa);
    assert_eq!((back["hops"].as_i64(), &back["thread"]), (Some(1), &thread));
    send(&b, &sb, &sc, "you take it", "r2", None);
    let fwd = take_one(&a, &sc);
    assert_eq!(fwd["hops"], 1);
    assert_eq!(fwd["thread"], fwd["id"], "a new thread");
    assert_ne!(fwd["thread"], thread);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leave_reaches_a_peer_or_warns_after_5s() {
    let (a, b) = pair(&[]);
    let t = Instant::now();
    let r = person(&b, json!({"id": 1, "op": "team.leave"}));
    assert_eq!(
        (r["ok"].as_bool(), r["told"].as_bool()),
        (Some(true), Some(true)),
        "{r}"
    );
    assert!(t.elapsed() < Duration::from_secs(5));
    let members = || {
        a.ask(json!({"id": 1, "op": "team.get"}))["members"]
            .as_array()
            .unwrap()
            .len()
    };
    until(5, "A sees B gone", || members() == 1);

    // Nobody online to tell: it leaves after 5 s and says so.
    let (d, e) = (Env::new(), Env::new());
    seed_pair(&d, &e);
    e.daemon(false, "", &[]);
    let t = Instant::now();
    let o = e.detached(&[EXE, "leave"], "y\n");
    assert!(o.status.success(), "{}", err(&o));
    assert!(t.elapsed() >= Duration::from_secs(5));
    assert!(
        out(&o).ends_with(
            "Left \"acme\". This computer is on its own network again.\n\
             Could not tell the team; they will see you gone when you are removed.\n"
        ),
        "{}",
        out(&o)
    );
}
