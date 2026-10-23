//! Hold notices and relayed decisions: a computer tells its own workspace's member of every new
//! hold (ids, never text), through the outbox; the person's phone/web tap comes back as a
//! signed `decision` carrying `decided_by`, applied only from that member, for that person.
//!
//! The world: holder B ("mbp", person Q) signed into W1 (or into nothing: a free user); A
//! ("mac", person P), a W1 teammate, played by the test; W1's member (an instance, or played)
//! and W2's member (played). One roster, seeded in every store before anything starts.

mod common;

use common::{admit, call, endpoint, now_s, person, person_proof, until, Env, Peer};
use lightbulb_connect::admission;
use lightbulb_connect::embed::Instance;
use lightbulb_connect::identity::Identity;
use lightbulb_connect::net::frames::Frame;
use lightbulb_connect::roster::{self, Entry, Kind};
use lightbulb_connect::store::Store;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::broadcast::Receiver;

const NAME: &str = "The Lightbulb Company";
const P: &str = "p-prasiddha";
const Q: &str = "p-quinn";

struct World {
    b: Env,
    a: Identity,
    /// R's computer ("mbc") in W2, played.
    c: Identity,
    w1: tempfile::TempDir,
    w2: tempfile::TempDir,
}

/// `who` played by the test, connected to `env`'s daemon.
async fn played(who: &Identity, env: &Env) -> (Peer, iroh::Endpoint) {
    let ep = endpoint(who).await;
    let mut p = Peer::dial(&ep, env).await;
    p.hello(vec![]).await;
    p.presence().await;
    (p, ep)
}

/// The world, holder B signed into `holder_ws` (`None`: a free user).
fn world(holder_ws: Option<&str>) -> World {
    world_at(holder_ws, now_s())
}

/// `world`, W1's member's proof issued at `iat` (two days back: lapsed).
fn world_at(holder_ws: Option<&str>, iat: i64) -> World {
    let b = Env::new();
    let (w1, w2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let id = |d: &tempfile::TempDir| Identity::load_or_create(d.path()).unwrap();
    let (ib, ia, iw1, iw2) = (b.identity(), common::identity(), id(&w1), id(&w2));
    let ic = common::identity();
    let g = Entry::signed(
        &ib,
        vec![],
        Kind::Admit,
        json!({"member": ib.key, "name": "quinn", "kind": "computer", "role": "founder",
            "ticket_id": "", "team_name": "acme", "person_id": Q, "computer": "mbp",
            "proof": person_proof(&ib, Q, "Quinn", holder_ws)}),
        1,
    );
    let e1 = admit(
        &ib,
        &g,
        &ia,
        "prasiddha",
        json!({"person_id": P, "computer": "mac",
            "proof": person_proof(&ia, P, "Prasiddha", Some("W1"))}),
    );
    let ws = |who: &Identity, id: &str, name: &str, iat| {
        json!({"kind": "cloud", "person_id": format!("ws:{id}"),
            "proof": common::issuer::workspace_proof(&who.key, id, name, &[], iat)})
    };
    let e2 = admit(&ib, &e1, &iw1, NAME, ws(&iw1, "W1", NAME, iat));
    let e3 = admit(
        &ib,
        &e2,
        &iw2,
        "Other Co",
        ws(&iw2, "W2", "Other Co", now_s()),
    );
    let e4 = admit(
        &ib,
        &e3,
        &ic,
        "rae",
        json!({"person_id": "p-rae", "computer": "mbc",
            "proof": person_proof(&ic, "p-rae", "Rae", Some("W2"))}),
    );
    for d in [&w1, &w2] {
        std::fs::set_permissions(
            d.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
    }
    let stores = [
        b.store(),
        Store::open(&w1.path().join("state.db")).unwrap(),
        Store::open(&w2.path().join("state.db")).unwrap(),
    ];
    for s in &stores {
        for e in [&g, &e1, &e2, &e3, &e4] {
            roster::accept(s, e, 10).unwrap();
        }
    }
    World {
        b,
        a: ia,
        c: ic,
        w1,
        w2,
    }
}

impl World {
    fn w1_id(&self) -> Identity {
        Identity::load_or_create(self.w1.path()).unwrap()
    }

    fn w2_id(&self) -> Identity {
        Identity::load_or_create(self.w2.path()).unwrap()
    }

    /// B's session `b1`.
    fn target(&self) -> String {
        common::session(&self.b, "b1")
    }

    /// A message from `from` (played `p`) to B's `b1`, held there: its envelope id.
    async fn held_from(&self, p: &mut Peer, from: &str, n: usize) -> String {
        let id = common::ulid();
        p.send(&envelope(&id, from, &self.target())).await;
        let ack = p
            .recv_where(|f| matches!(f, Frame::Ack { id: i, .. } if *i == id))
            .await;
        assert!(
            matches!(&ack, Frame::Ack { state, .. } if state == "held"),
            "{ack:?}"
        );
        until(10, "the hold", || open_holds(&self.b).len() == n);
        id
    }

    /// W1's instance, dialing B, and its events from the start, once B is online to it.
    async fn instance(&self, knobs: &[(&str, String)]) -> (Instance, Receiver<Value>) {
        let i = common::instance(
            self.w1.path(),
            &[&self.b],
            NAME,
            "http://127.0.0.1:9/connect",
            None,
            knobs,
        )
        .await
        .unwrap();
        let rx = i.events();
        online(&i).await;
        (i, rx)
    }
}

/// `p` (`who`, a workspace member) declares its session `name`, so it may send from it.
async fn declare(p: &mut Peer, who: &Identity, name: &str) {
    let mut r = common::remote(&format!("{}/{name}", who.short));
    r.agent = "Friday".into();
    let seq = now_s() as u64 * 1000 + 999;
    // Read before any later frame on the stream.
    p.send(&common::presence_with(&who.key, seq, vec![r])).await;
}

fn envelope(id: &str, from: &str, to: &str) -> Frame {
    Frame::Envelope {
        id: id.into(),
        request_id: common::ulid(),
        kind: "message".into(),
        from: from.into(),
        to: to.into(),
        reply_to: None,
        thread: None,
        hops: 0,
        body: "the secret plan".into(),
        files: vec![],
        sent_at: now_s(),
    }
}

async fn online(i: &Instance) {
    for _ in 0..500 {
        if call(i, "status", json!({})).await["peers"]["online"] == 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("B online");
}

fn open_holds(e: &Env) -> Vec<Value> {
    person(e, json!({"id": 1, "op": "holds"}))["holds"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn hold_state(e: &Env, hold: &str) -> String {
    let r = person(e, json!({"id": 1, "op": "hold.get", "hold": hold}));
    r["state"].as_str().unwrap_or_default().into()
}

/// A decision signed by `who`, for B's hold `hold`.
fn decision(who: &Identity, b: &Env, hold: &str, scope: &str, decided_by: &str) -> Frame {
    decision_with(who, b, hold, scope, decided_by, &common::ulid())
}

fn decision_with(
    who: &Identity,
    b: &Env,
    hold: &str,
    scope: &str,
    decided_by: &str,
    nonce: &str,
) -> Frame {
    admission::signed(
        who,
        Frame::Decision {
            hold_id: hold.into(),
            holding_member: b.identity().key,
            verdict: "allow".into(),
            scope: scope.into(),
            decided_by: decided_by.into(),
            nonce: nonce.into(),
            decided_at: now_s(),
            sig: String::new(),
        },
    )
}

fn nonce_of(f: &Frame) -> String {
    match f {
        Frame::Decision { nonce, .. } => nonce.clone(),
        _ => unreachable!(),
    }
}

/// The ack of decision `f`: its state.
async fn decision_ack(p: &mut Peer, f: &Frame) -> String {
    let (state, reason) = ack_of(p, f).await;
    assert_eq!(reason, None);
    state
}

/// The ack of decision `f`: its state and reason.
async fn ack_of(p: &mut Peer, f: &Frame) -> (String, Option<String>) {
    let want = format!("dc:{}", nonce_of(f));
    match p
        .recv_where(|x| matches!(x, Frame::Ack { id, .. } if *id == want))
        .await
    {
        Frame::Ack { state, reason, .. } => (state, reason),
        _ => unreachable!(),
    }
}

/// Decision `f` sent by `p` is refused for `why`, acked, its hold `hold` still open.
async fn refused(w: &World, p: &mut Peer, f: &Frame, hold: &str, why: &str) {
    p.send(f).await;
    let got = ack_of(p, f).await;
    assert_eq!(got, ("refused".into(), Some(why.into())));
    assert_eq!(hold_state(&w.b, hold), "open");
}

/// The next hold notice `p` gets, if one comes within `secs`.
async fn notice(p: &mut Peer, secs: u64) -> Option<Frame> {
    tokio::time::timeout(
        Duration::from_secs(secs),
        p.recv_where(|f| matches!(f, Frame::HoldNotice { .. })),
    )
    .await
    .ok()
}

/// A decision `f` sent by `p` is left unanswered: no ack in a second, the hold still open.
async fn unanswered(w: &World, p: &mut Peer, f: &Frame, hold: &str) {
    p.send(f).await;
    let want = format!("dc:{}", nonce_of(f));
    let acked = tokio::time::timeout(
        Duration::from_secs(1),
        p.recv_where(|x| matches!(x, Frame::Ack { id, .. } if *id == want)),
    )
    .await;
    assert!(acked.is_err(), "acked: {acked:?}");
    assert_eq!(hold_state(&w.b, hold), "open");
}

/// The next instance event called `name`.
async fn event(rx: &mut Receiver<Value>, name: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let v = rx.recv().await.unwrap();
            if v["event"] == name {
                return v;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("a {name} event"))
}

fn count(db: &std::path::Path, sql: &str) -> i64 {
    let c = rusqlite::Connection::open(db).unwrap();
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn people(e: &Env) -> Value {
    e.ask(json!({"id": 1, "op": "policy.get"}))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_allow_once_delivers_and_writes_no_policy() {
    let w = world(Some("W1"));
    w.b.daemon(false, "", &[]);
    let (mut m, _e1) = played(&w.w1_id(), &w.b).await;
    let (mut pa, _e2) = played(&w.a, &w.b).await;
    let from = format!("{}/codex:a1", w.a.short);
    w.held_from(&mut pa, &from, 1).await;
    let n = notice(&mut m, 5).await.expect("a notice");
    let Frame::HoldNotice {
        hold_id,
        kind,
        holding_computer,
        for_person,
        target_session,
        from: f,
        preview,
        ..
    } = &n
    else {
        unreachable!()
    };
    assert_eq!(
        (hold_id.as_str(), kind.as_str(), holding_computer.as_str()),
        ("H1", "message", "mbp")
    );
    assert_eq!((for_person.as_str(), f.as_str()), (Q, from.as_str()));
    assert_eq!(
        (target_session.as_str(), preview.as_str()),
        (w.target().as_str(), "")
    );
    let v = serde_json::to_value(&n).unwrap();
    let sig = v["sig"].as_str().unwrap().to_string();
    assert!(lightbulb_connect::identity::verify(
        &w.b.identity().key,
        &v,
        &sig
    ));
    let d = decision(&w.w1_id(), &w.b, "H1", "once", Q);
    m.send(&d).await;
    assert_eq!(decision_ack(&mut m, &d).await, "applied");
    assert_eq!(hold_state(&w.b, "H1"), "allowed");
    assert_eq!(w.b.take_one(&w.target())["text"], "the secret plan");
    let p = people(&w.b);
    assert_eq!(p["people"], json!({}), "{p}");
    assert_eq!(p["agents_send_without_asking"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decision_from_another_workspaces_member_is_refused() {
    let w = world(Some("W1"));
    w.b.daemon(false, "", &[]);
    let (mut other, _e1) = played(&w.w2_id(), &w.b).await;
    let (mut pa, _e2) = played(&w.a, &w.b).await;
    w.held_from(&mut pa, &format!("{}/codex:a1", w.a.short), 1)
        .await;
    let d = decision(&w.w2_id(), &w.b, "H1", "once", Q);
    unanswered(&w, &mut other, &d, "H1").await;
    // A computer signer: never served.
    let d = decision(&w.a, &w.b, "H1", "once", Q);
    unanswered(&w, &mut pa, &d, "H1").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decision_decided_by_another_person_is_refused() {
    let w = world(Some("W1"));
    w.b.daemon(false, "", &[]);
    let (mut m, _e1) = played(&w.w1_id(), &w.b).await;
    let (mut pa, _e2) = played(&w.a, &w.b).await;
    w.held_from(&mut pa, &format!("{}/codex:a1", w.a.short), 1)
        .await;
    // P is a W1 teammate, not the holding computer's person.
    let d = decision(&w.w1_id(), &w.b, "H1", "once", P);
    refused(&w, &mut m, &d, "H1", "not_mine").await;
    // Unsigned (signed by someone else) is refused too.
    let mut forged = decision(&w.w2_id(), &w.b, "H1", "once", Q);
    if let Frame::Decision { holding_member, .. } = &mut forged {
        assert_eq!(*holding_member, w.b.identity().key);
    }
    refused(&w, &mut m, &forged, "H1", "bad_sig").await;
    // A hold id that exists only on another computer.
    let d = decision(&w.w1_id(), &w.b, "H9", "once", Q);
    refused(&w, &mut m, &d, "H1", "not_mine").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn instance_refuses_decision_send_for_another_person() {
    let w = world(Some("W1"));
    w.b.daemon(false, "", &[]);
    let (i, _rx) = w.instance(&[]).await;
    let holding = w.b.identity().key;
    for (decided_by, scope) in [
        (P, "once"),
        ("nobody", "once"),
        (Q, "cloud_of"),
        (Q, "always"),
    ] {
        let r = call(
            &i,
            "decision.send",
            json!({"hold_id": "H1", "holding_member": holding, "verdict": "allow",
                "scope": scope, "decided_by": decided_by}),
        )
        .await;
        assert_eq!(r["error"], "bad_request", "{decided_by} {scope}: {r}");
    }
    let db = w.w1.path().join("state.db");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM outbox"), 0);
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relayed_cloud_of_is_refused() {
    let w = world(Some("W1"));
    w.b.daemon(false, "", &[]);
    let (mut m, _e1) = played(&w.w1_id(), &w.b).await;
    let (mut other, _e2) = played(&w.w2_id(), &w.b).await;
    // W2's agent writes to B: held; W1's member asks to always allow W2.
    declare(&mut other, &w.w2_id(), "friday").await;
    w.held_from(&mut other, &format!("{}/friday", w.w2_id().short), 1)
        .await;
    for scope in ["cloud_of", "always"] {
        let d = decision(&w.w1_id(), &w.b, "H1", scope, Q);
        refused(&w, &mut m, &d, "H1", "scope").await;
    }
    // Nor `person` on another workspace's member: it would be `cloud_of`.
    let d = decision(&w.w1_id(), &w.b, "H1", "person", Q);
    refused(&w, &mut m, &d, "H1", "scope").await;
    assert_eq!(people(&w.b)["cloud_of"], json!({}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replayed_nonce_is_refused() {
    let w = world(Some("W1"));
    w.b.daemon(false, "", &[]);
    let (mut m, _e1) = played(&w.w1_id(), &w.b).await;
    let (mut pa, _e2) = played(&w.a, &w.b).await;
    let from = format!("{}/codex:a1", w.a.short);
    w.held_from(&mut pa, &from, 1).await;
    let d = decision(&w.w1_id(), &w.b, "H1", "once", Q);
    m.send(&d).await;
    assert_eq!(decision_ack(&mut m, &d).await, "applied");
    // The same frame again (its ack was lost): the same answer, applied once.
    m.send(&d).await;
    assert_eq!(decision_ack(&mut m, &d).await, "applied");
    assert_eq!(w.b.unread(&w.target()), 1);
    // Its nonce on another hold: refused, not applied.
    w.held_from(&mut pa, &from, 1).await;
    let again = decision_with(&w.w1_id(), &w.b, "H2", "once", Q, &nonce_of(&d));
    refused(&w, &mut m, &again, "H2", "bad_request").await;
    // A refusal is kept too: its copy is refused the same.
    let bad = decision(&w.w1_id(), &w.b, "H2", "cloud_of", Q);
    refused(&w, &mut m, &bad, "H2", "scope").await;
    refused(&w, &mut m, &bad, "H2", "scope").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn own_cloud_scope_only_for_the_members_own_session() {
    let w = world(Some("W1"));
    w.b.daemon(false, "", &[]);
    let (mut m, _e1) = played(&w.w1_id(), &w.b).await;
    let (mut pa, _e2) = played(&w.a, &w.b).await;
    // A teammate's message: own_cloud refused, person allowed (set by the bridge).
    w.held_from(&mut pa, &format!("{}/codex:a1", w.a.short), 1)
        .await;
    let d = decision(&w.w1_id(), &w.b, "H1", "own_cloud", Q);
    refused(&w, &mut m, &d, "H1", "scope").await;
    let d = decision(&w.w1_id(), &w.b, "H1", "person", Q);
    m.send(&d).await;
    assert_eq!(decision_ack(&mut m, &d).await, "applied");
    let p = people(&w.b);
    assert_eq!(p["people"][P], "accept", "{p}");
    assert_eq!(p["set_by"][format!("people:{P}")], "bridge");
    let o = w.b.detached(&[common::EXE, "policy"], "");
    assert!(
        common::out(&o).contains("Allowed prasiddha from your phone"),
        "{}",
        common::out(&o)
    );
    // The member's own agent's message: own_cloud turns the switch on.
    declare(&mut m, &w.w1_id(), "friday").await;
    w.held_from(&mut m, &format!("{}/friday", w.w1_id().short), 1)
        .await;
    let d = decision(&w.w1_id(), &w.b, "H2", "own_cloud", Q);
    m.send(&d).await;
    assert_eq!(decision_ack(&mut m, &d).await, "applied");
    let p = people(&w.b);
    assert_eq!(p["agents_send_without_asking"], true);
    assert_eq!(p["set_by"]["agents_send_without_asking"], "bridge");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notice_goes_only_to_the_holders_workspace_member_and_carries_no_text() {
    let w = world(Some("W1"));
    w.b.daemon(false, "preview_to_workspace = true\n", &[]);
    let (mut m, _e1) = played(&w.w1_id(), &w.b).await;
    let (mut other, _e2) = played(&w.w2_id(), &w.b).await;
    let (mut pa, _e3) = played(&w.a, &w.b).await;
    w.held_from(&mut pa, &format!("{}/codex:a1", w.a.short), 1)
        .await;
    let n = notice(&mut m, 5).await.expect("W1 gets it");
    let text = serde_json::to_string(&n).unwrap();
    assert!(!text.contains("secret"), "{text}");
    assert!(matches!(&n, Frame::HoldNotice { preview, .. } if preview.is_empty()));
    assert!(notice(&mut other, 2).await.is_none(), "W2 gets nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn free_user_hold_sends_no_notice() {
    let w = world(None);
    w.b.daemon(false, "", &[]);
    let (mut m, _e1) = played(&w.w1_id(), &w.b).await;
    let (mut pa, _e2) = played(&w.a, &w.b).await;
    w.held_from(&mut pa, &format!("{}/codex:a1", w.a.short), 1)
        .await;
    assert!(notice(&mut m, 2).await.is_none());
    let db = w.b.state.path().join("state.db");
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM outbox WHERE message_id LIKE 'hn:%'"
        ),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hold_notice_reaches_a_member_that_was_offline() {
    let w = world(Some("W1"));
    w.b.daemon(false, "", &[]);
    let (mut pa, _e) = played(&w.a, &w.b).await;
    let from = format!("{}/codex:a1", w.a.short);
    w.held_from(&mut pa, &from, 1).await;
    let (i, mut rx) = w.instance(&[]).await;
    let e = event(&mut rx, "hold_notice").await;
    assert_eq!(
        (e["hold_id"].as_str(), e["for_person"].as_str()),
        (Some("H1"), Some(Q))
    );
    let db = w.b.state.path().join("state.db");
    until(5, "acked", || {
        count(&db, "SELECT COUNT(*) FROM outbox") == 0
    });
    i.shutdown().await;
    // A hold answered while the member is down: its notice never goes.
    w.held_from(&mut pa, &from, 2).await;
    let r = person(
        &w.b,
        json!({"id": 1, "op": "hold.answer", "hold": "H2", "verdict": "decline"}),
    );
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM outbox WHERE message_id LIKE 'hn:%'"
        ),
        0
    );
    let (i, mut rx) = w.instance(&[]).await;
    let quiet = tokio::time::timeout(Duration::from_secs(3), event(&mut rx, "hold_notice")).await;
    assert!(quiet.is_err(), "{quiet:?}");
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decision_reaches_a_holder_that_was_offline() {
    let w = world(Some("W1"));
    w.b.daemon(false, "", &[]);
    let (mut pa, _e) = played(&w.a, &w.b).await;
    w.held_from(&mut pa, &format!("{}/codex:a1", w.a.short), 1)
        .await;
    drop(pa);
    let (i, mut rx) = w.instance(&[]).await;
    event(&mut rx, "hold_notice").await;
    // B goes down; the tap comes meanwhile.
    w.b.ask(json!({"id": 1, "op": "shutdown"}));
    until(10, "B down", || {
        std::os::unix::net::UnixStream::connect(w.b.state.path().join("sock")).is_err()
    });
    let r = call(
        &i,
        "decision.send",
        json!({"hold_id": "H1", "holding_member": w.b.identity().key, "verdict": "allow",
            "scope": "once", "decided_by": Q}),
    )
    .await;
    assert_eq!(r["ok"], true, "{r}");
    let db = w.w1.path().join("state.db");
    let frame: String = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT frame FROM outbox WHERE message_id LIKE 'dc:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let copy: Frame = serde_json::from_str(&frame).unwrap();
    // B comes back (dialing the instance): the decision arrives and is applied, once.
    let port = call(&i, "status", json!({})).await["port"].clone();
    let peers = format!("{}@127.0.0.1:{port}", w.w1_id().key);
    w.b.daemon(false, "", &[("LIGHTBULB_CONNECT_PEERS", peers)]);
    let e = event(&mut rx, "decision_result").await;
    assert_eq!(
        (e["hold_id"].as_str(), e["result"].as_str()),
        (Some("H1"), Some("applied"))
    );
    assert_eq!(hold_state(&w.b, "H1"), "allowed");
    assert_eq!(w.b.unread(&w.target()), 1);
    i.shutdown().await;
    // A second copy of the same decision (its ack lost): acked `applied` again, nothing more
    // delivered.
    let (mut m, _e1) = played(&w.w1_id(), &w.b).await;
    m.send(&copy).await;
    assert_eq!(decision_ack(&mut m, &copy).await, "applied");
    assert_eq!(w.b.unread(&w.target()), 1);
}

/// Carried from Task 11b.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hold_notice_and_decision_round_trip_on_an_instance() {
    let w = world(Some("W1"));
    w.b.daemon(false, "", &[]);
    let (i, mut rx) = w.instance(&[]).await;
    let (mut pa, _e) = played(&w.a, &w.b).await;
    let from = format!("{}/codex:a1", w.a.short);
    w.held_from(&mut pa, &from, 1).await;
    let e = event(&mut rx, "hold_notice").await;
    let holding = w.b.identity().key;
    assert_eq!(e["hold_id"], "H1");
    assert_eq!(e["holding_member"], holding.as_str());
    assert_eq!(e["holding_computer"], "mbp");
    assert_eq!(e["for_person"], Q);
    assert_eq!(e["target_session"], w.target());
    assert_eq!(e["from"], from);
    assert_eq!(e["kind"], "message");
    assert!(e["label"].as_str().is_some_and(|l| !l.is_empty()), "{e}");
    assert!(e["created_at"].is_i64(), "{e}");
    assert!(!e.to_string().contains("secret"), "{e}");
    let send = || {
        call(
            &i,
            "decision.send",
            json!({"hold_id": "H1", "holding_member": holding, "verdict": "allow",
                "scope": "once", "decided_by": Q}),
        )
    };
    assert_eq!(send().await["ok"], true);
    let r = event(&mut rx, "decision_result").await;
    assert_eq!(
        (r["hold_id"].as_str(), r["result"].as_str()),
        (Some("H1"), Some("applied"))
    );
    assert_eq!(hold_state(&w.b, "H1"), "allowed");
    assert_eq!(send().await["ok"], true);
    let r = event(&mut rx, "decision_result").await;
    assert_eq!(r["result"], "not_open");
    // A permission hold rides the same path; Claude reads `hold_closed` by the bridge.
    let me = lightbulb_connect::os::process(std::process::id()).unwrap();
    let r = w.b.ask(
        json!({"id": 1, "op": "register", "session": {"provider": "claude",
        "conversation": "c1", "pid": me.pid, "birth_micros": me.birth_micros, "cwd": "/tmp"}}),
    );
    let session = r["key"].as_str().unwrap().to_string();
    let filed = person(
        &w.b,
        json!({"id": 1, "op": "hold.file", "kind": "permission", "session": session,
            "prompt": "Run rm -rf build?"}),
    );
    assert_eq!(filed["ok"], true, "{filed}");
    let hold = filed["hold"].as_str().unwrap().to_string();
    let e = event(&mut rx, "hold_notice").await;
    assert_eq!(
        (e["hold_id"].as_str(), e["kind"].as_str()),
        (Some(hold.as_str()), Some("permission"))
    );
    assert!(!e.to_string().contains("rm -rf"), "{e}");
    let mut sub = subscribe(&w.b);
    let r = call(
        &i,
        "decision.send",
        json!({"hold_id": hold, "holding_member": holding, "verdict": "allow",
            "scope": "once", "decided_by": Q}),
    )
    .await;
    assert_eq!(r["ok"], true, "{r}");
    let closed = next_event(&mut sub, "hold_closed");
    assert_eq!(
        (
            closed["id"].as_str(),
            closed["by"].as_str(),
            closed["verdict"].as_str()
        ),
        (Some(hold.as_str()), Some("bridge"), Some("allow"))
    );
    i.shutdown().await;
}

fn subscribe(env: &Env) -> std::io::BufReader<std::os::unix::net::UnixStream> {
    use std::io::{BufRead, Write};
    let mut s = std::os::unix::net::UnixStream::connect(env.state.path().join("sock")).unwrap();
    writeln!(s, r#"{{"id":1,"op":"subscribe"}}"#).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut r = std::io::BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    r
}

fn next_event(r: &mut std::io::BufReader<std::os::unix::net::UnixStream>, name: &str) -> Value {
    use std::io::BufRead;
    loop {
        let mut line = String::new();
        r.read_line(&mut line).expect("an event within 10 s");
        let v: Value = serde_json::from_str(&line).unwrap();
        if v["event"] == name {
            return v;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lapsed_member_is_left_unanswered() {
    let w = world_at(Some("W1"), now_s() - 2 * 86_400);
    w.b.daemon(false, "", &[]);
    let (mut m, _e1) = played(&w.w1_id(), &w.b).await;
    let (mut pa, _e2) = played(&w.a, &w.b).await;
    w.held_from(&mut pa, &format!("{}/codex:a1", w.a.short), 1)
        .await;
    // No workspace member in force: no notice, and its decisions may heal, so no answer.
    assert!(notice(&mut m, 1).await.is_none());
    let d = decision(&w.w1_id(), &w.b, "H1", "once", Q);
    unanswered(&w, &mut m, &d, "H1").await;
}

/// A signed notice from `who` (member `short`'s computer `computer`) for `for_person`.
fn notice_from(who: &Identity, computer: &str, for_person: &str) -> Frame {
    admission::signed(
        who,
        Frame::HoldNotice {
            hold_id: "H1".into(),
            kind: "message".into(),
            holding_computer: computer.into(),
            for_person: for_person.into(),
            target_session: format!("{}/codex:x", who.short),
            from: format!("{}/codex:y", who.short),
            label: "Someone's Codex".into(),
            preview: String::new(),
            created_at: now_s(),
            sig: String::new(),
        },
    )
}

/// `who` played by the test, connected to the instance at `addr`.
async fn dial(addr: &iroh::EndpointAddr, who: &Identity) -> (Peer, iroh::Endpoint) {
    let ep = endpoint(who).await;
    let conn = ep
        .connect(addr.clone(), lightbulb_connect::net::ALPN)
        .await
        .unwrap();
    let (send, recv) = conn.open_bi().await.unwrap();
    let mut p = Peer { conn, send, recv };
    p.hello(vec![]).await;
    (p, ep)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_instance_drops_notices_not_from_its_own_persons_computers() {
    let w = world(Some("W1"));
    w.b.daemon(false, "", &[]);
    let (i, mut rx) = w.instance(&[]).await;
    let port = call(&i, "status", json!({})).await["port"]
        .as_u64()
        .unwrap();
    let key = lightbulb_connect::identity::key_bytes(&w.w1_id().key).unwrap();
    let addr = iroh::EndpointAddr::new(iroh::PublicKey::from_bytes(&key).unwrap())
        .with_ip_addr(format!("127.0.0.1:{port}").parse().unwrap());

    // R's computer in W2; P's computer naming Q; P's computer under another computer's name.
    let (mut pc, _e1) = dial(&addr, &w.c).await;
    pc.send(&notice_from(&w.c, "mbc", "p-rae")).await;
    let (mut pa, _e2) = dial(&addr, &w.a).await;
    pa.send(&notice_from(&w.a, "mac", Q)).await;
    pa.send(&notice_from(&w.a, "mbp", P)).await;
    // The one that is P's own, for P: the only event.
    pa.send(&notice_from(&w.a, "mac", P)).await;
    let e = event(&mut rx, "hold_notice").await;
    assert_eq!(
        (e["holding_member"].as_str(), e["for_person"].as_str()),
        (Some(w.a.key.as_str()), Some(P))
    );
    let dropped = call(&i, "status", json!({})).await["dropped"]["hold_notice"].clone();
    assert_eq!(dropped, 3, "{dropped}");
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusal_reaches_decision_result() {
    let w = world(Some("W1"));
    w.b.daemon(false, "", &[]);
    let (i, mut rx) = w.instance(&[]).await;
    let (mut pa, _e) = played(&w.a, &w.b).await;
    w.held_from(&mut pa, &format!("{}/codex:a1", w.a.short), 1)
        .await;
    event(&mut rx, "hold_notice").await;
    // `own_cloud` on a teammate's message.
    let r = call(
        &i,
        "decision.send",
        json!({"hold_id": "H1", "holding_member": w.b.identity().key, "verdict": "allow",
            "scope": "own_cloud", "decided_by": Q}),
    )
    .await;
    assert_eq!(r["ok"], true, "{r}");
    let e = event(&mut rx, "decision_result").await;
    assert_eq!(
        (
            e["hold_id"].as_str(),
            e["result"].as_str(),
            e["reason"].as_str()
        ),
        (Some("H1"), Some("refused"), Some("scope"))
    );
    assert_eq!(hold_state(&w.b, "H1"), "open");
    // Answered: no longer pending.
    assert_eq!(
        call(&i, "holds.pending", json!({})).await["notices"],
        json!([])
    );
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_notice_no_one_heard_is_not_acked_and_stays_pending() {
    let w = world(Some("W1"));
    w.b.daemon(false, "", &[]);
    // No subscriber yet.
    let i = common::instance(
        w.w1.path(),
        &[&w.b],
        NAME,
        "http://127.0.0.1:9/connect",
        None,
        &[],
    )
    .await
    .unwrap();
    online(&i).await;
    let (mut pa, _e) = played(&w.a, &w.b).await;
    w.held_from(&mut pa, &format!("{}/codex:a1", w.a.short), 1)
        .await;
    let mut pending = json!([]);
    for _ in 0..250 {
        pending = call(&i, "holds.pending", json!({})).await["notices"].clone();
        if pending != json!([]) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(pending[0]["hold_id"], "H1", "{pending}");
    let db = w.b.state.path().join("state.db");
    let unacked = "SELECT COUNT(*) FROM outbox WHERE message_id = 'hn:H1'";
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(count(&db, unacked), 1, "not acked without a subscriber");
    // The bridge attaches: the holder's next try is heard and acked.
    let mut rx = i.events();
    assert_eq!(event(&mut rx, "hold_notice").await["hold_id"], "H1");
    until(5, "acked", || count(&db, unacked) == 0);
    i.shutdown().await;
}
