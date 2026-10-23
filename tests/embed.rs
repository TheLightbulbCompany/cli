//! An embedded instance: the engine in process (no socket, launchd, hooks or dialog) as a
//! Lightbulb workspace's one `kind: cloud` member, whose sessions are the workspace's agents.

mod common;

use common::{dialog, endpoint, identity, now_s, peers, ulid, until, Env, Peer};
use lightbulb_connect::embed::{self, EmbedOpts, Instance};
use lightbulb_connect::identity::{key_bytes, Identity};
use lightbulb_connect::net::frames::Frame;
use lightbulb_connect::net::ALPN;
use lightbulb_connect::policy;
use lightbulb_connect::roster::{self, Entry, Kind, MemberKind};
use lightbulb_connect::store::Store;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use tokio::sync::broadcast::Receiver;
use tokio::sync::Mutex;

const NAME: &str = "The Lightbulb Company";

/// A workspace proof for `ws` (opaque until Task 13 verifies it).
fn proof(ws: &str) -> String {
    json!({"kind": "workspace", "sub": format!("ws:{ws}"),
        "workspace": {"id": ws, "name": NAME}})
    .to_string()
}

/// `LIGHTBULB_CONNECT_PEERS` is read from this process's environment when an instance
/// starts: one start at a time.
static STARTING: Mutex<()> = Mutex::const_new(());

/// An instance on `home` that dials `envs` at their running daemons, with a final snapshot to
/// `snapshot` at shutdown.
async fn start(home: &Path, envs: &[&Env], snapshot: Option<&Path>) -> Instance {
    let _one = STARTING.lock().await;
    std::env::set_var("LIGHTBULB_CONNECT_PEERS", peers(envs).1);
    let opts = EmbedOpts {
        kind: MemberKind::Cloud,
        relay: None,
        issuer: "http://127.0.0.1:9/connect".into(),
        snapshot: snapshot.map(Path::to_path_buf),
    };
    let i = embed::start(home, opts).await.unwrap();
    std::env::remove_var("LIGHTBULB_CONNECT_PEERS");
    i
}

async fn call(i: &Instance, op: &str, body: Value) -> Value {
    let mut req = body;
    req["id"] = 1.into();
    req["op"] = op.into();
    i.call(req).await
}

async fn ok(i: &Instance, op: &str, body: Value) -> Value {
    let r = call(i, op, body).await;
    assert_eq!(r["ok"], true, "{op}: {r}");
    r
}

fn refused(r: &Value, reason: &str) {
    assert_eq!(
        (r["error"].as_str(), r["reason"].as_str()),
        (Some("bad_request"), Some(reason)),
        "{r}"
    );
}

/// The next event `pred` holds for, within 10 s.
async fn event(rx: &mut Receiver<Value>, pred: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let e = rx.recv().await.unwrap();
            if pred(&e) {
                return e;
            }
        }
    })
    .await
    .expect("the event within 10 s")
}

/// A genesis by `id` ("prasiddha" on "mac", team "acme") signed into workspace `ws`.
fn genesis(id: &Identity, ws: Option<&str>) -> Entry {
    let body = json!({"member": id.key, "name": "prasiddha", "computer": "mac",
        "person_id": id.key, "kind": "computer", "role": "founder", "ticket_id": "",
        "team_name": "acme", "workspace": ws});
    Entry::signed(id, vec![], Kind::Admit, body, 1)
}

/// A running computer A in workspace W1, founder of its own network, whose dialogs answer
/// Allow and whose own workspace's member may write without a hold.
fn computer_a() -> Env {
    let a = Env::new();
    let store = a.store();
    roster::accept(&store, &genesis(&a.identity(), Some("W1")), 10).unwrap();
    policy::set_switch(&store, true, "local", 1).unwrap();
    a.daemon(false, "", &[dialog("Allow")]);
    a
}

async fn join(i: &Instance, a: &Env) -> Value {
    let t = a.ask(json!({"id": 1, "op": "team.invite"}));
    let ticket = t["ticket"].as_str().unwrap();
    ok(
        i,
        "team.join",
        json!({"ticket": ticket, "name": NAME, "kind": "cloud", "proof": proof("W1")}),
    )
    .await
}

async fn declare(i: &Instance, names: &[&str]) {
    let s: Vec<Value> = names
        .iter()
        .map(|n| json!({"name": n, "agent": n.to_uppercase(), "title": format!("{n} agent")}))
        .collect();
    ok(i, "cloud.sessions.set", json!({ "sessions": s })).await;
}

async fn me(i: &Instance) -> String {
    ok(i, "identity.get", json!({})).await["member_key_short"]
        .as_str()
        .unwrap()
        .into()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn embedded_instance_joins_and_talks_without_a_socket() {
    let a = computer_a();
    let home = tempfile::tempdir().unwrap();
    let i = start(home.path(), &[&a], None).await;
    let mut events = i.events();
    let j = join(&i, &a).await;
    assert_eq!(j["state"], "admitted", "{j}");
    assert!(!home.path().join("sock").exists());
    declare(&i, &["friday"]).await;
    let w = me(&i).await;
    let friday = format!("{w}/friday");
    until(10, "A sees friday", || common::sees(&a, &friday));
    let dir = a.ask(json!({"id": 1, "op": "directory"}));
    let rec = dir["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == friday.as_str())
        .cloned()
        .unwrap();
    assert_eq!(rec["person_id"], "ws:W1");
    assert_eq!(rec["agent"], "FRIDAY");

    let sa = common::session(&a, "a1");
    let r = ok(
        &i,
        "send",
        json!({"from": friday, "to": sa, "text": "hello from friday", "request_id": "f1"}),
    )
    .await;
    assert_eq!(r["receipt"]["request_id"], "f1");
    assert!(r["receipt"]["thread"].is_string(), "{r}");
    let m = a.take_one(&sa);
    assert_eq!(m["from"], friday.as_str());

    a.send(&sa, &friday, "hi back", "a1", m["id"].as_str());
    let e = event(&mut events, |e| e["event"] == "message").await;
    assert_eq!(e["to_session"], "friday");
    let got = ok(&i, "inbox", json!({"session": "friday"})).await;
    let msgs = got["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1, "{got}");
    assert_eq!(msgs[0]["id"], e["id"]);
    assert_eq!(msgs[0]["text"], "hi back");
    let keys: Vec<&String> = msgs[0].as_object().unwrap().keys().collect();
    for k in [
        "id",
        "request_id",
        "thread",
        "from",
        "label",
        "person",
        "person_verified",
        "computer",
        "text",
        "sent_at",
    ] {
        assert!(keys.iter().any(|x| *x == k), "{k} in {keys:?}");
    }
    until(10, "A's reply reads read", || a.state(&sa, "a1") == "read");
    assert!(
        ok(&i, "inbox", json!({"session": "friday"})).await["messages"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn person_only_ops_are_refused_in_process() {
    let home = tempfile::tempdir().unwrap();
    let i = start(home.path(), &[], None).await;
    let x = identity().key;
    for (op, body) in [
        ("team.remove", json!({"member": x})),
        ("team.admin", json!({"member": x})),
        ("hold.answer", json!({"hold": "H1", "verdict": "allow"})),
        ("hold.get", json!({"hold": "H1"})),
        ("team.invite", json!({"mode": "open"})),
        ("policy.set", json!({"person_id": x, "value": "accept"})),
        ("share.grant", json!({"session": "x", "to": x})),
    ] {
        refused(&call(&i, op, body).await, "not_a_person");
    }
    assert_eq!(
        call(&i, "team.unlink", json!({})).await["error"],
        "unsupported"
    );
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undeclared_cloud_session_cannot_send() {
    let home = tempfile::tempdir().unwrap();
    let i = start(home.path(), &[], None).await;
    declare(&i, &["friday"]).await;
    let w = me(&i).await;
    let r = call(
        &i,
        "send",
        json!({"from": format!("{w}/ghost"), "to": format!("{w}/friday"),
        "text": "boo", "request_id": "g1"}),
    )
    .await;
    assert_eq!(r["error"], "bad_request", "{r}");
    // A computer has no declared sessions to set.
    let e = Env::new();
    e.daemon(false, "", &[]);
    let r = e.ask(json!({"id": 1, "op": "cloud.sessions.set", "sessions": []}));
    assert_eq!(r["error"], "unsupported", "{r}");
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_instance_joins_and_leaves_in_process() {
    let a = computer_a();
    let home = tempfile::tempdir().unwrap();
    let i = start(home.path(), &[&a], None).await;
    // Only as the workspace member.
    let t = a.ask(json!({"id": 1, "op": "team.invite"}));
    let r = call(
        &i,
        "team.join",
        json!({"ticket": t["ticket"], "name": NAME, "kind": "computer"}),
    )
    .await;
    assert_eq!(r["error"], "bad_request", "{r}");
    assert_eq!(join(&i, &a).await["state"], "admitted");
    let key = ok(&i, "identity.get", json!({})).await["member_key"].clone();
    let team = ok(&i, "team.get", json!({})).await;
    let mine = team["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["key"] == key)
        .cloned()
        .unwrap();
    assert_eq!(
        (
            mine["person_id"].as_str(),
            mine["workspace"].as_str(),
            mine["kind"].as_str()
        ),
        (Some("ws:W1"), Some("W1"), Some("cloud"))
    );
    refused(
        &call(&i, "team.remove", json!({"member": a.identity().key})).await,
        "not_a_person",
    );
    let left = ok(&i, "team.leave", json!({})).await;
    assert_eq!(left["told"], true, "{left}");
    let team = ok(&i, "team.get", json!({})).await;
    assert_eq!(team["members"].as_array().unwrap().len(), 1, "{team}");
    i.shutdown().await;
}

/// The three members an instance's test plays: identity, connection, endpoint.
type Played = (Identity, Peer, iroh::Endpoint);

/// An instance whose stored team (before it starts) is A (founder, W1), B (W2), C (no
/// workspace) and itself, W1's member; returns its home, it and the three, connected.
async fn seeded(snapshot: Option<&Path>) -> (tempfile::TempDir, Instance, [Played; 3]) {
    let home = tempfile::tempdir().unwrap();
    let w = Identity::load_or_create(home.path()).unwrap();
    let (ia, ib, ic) = (identity(), identity(), identity());
    let g = genesis(&ia, Some("W1"));
    let ab = common::admit(&ia, &g, &ib, "bob", json!({"workspace": "W2"}));
    let ac = common::admit(&ia, &ab, &ic, "carol", json!({}));
    let aw = common::admit(
        &ia,
        &ac,
        &w,
        NAME,
        json!({"kind": "cloud", "person_id": "ws:W1",
        "workspace": "W1", "proof": proof("W1")}),
    );
    let store = Store::open(&home.path().join("state.db")).unwrap();
    for e in [&g, &ab, &ac, &aw] {
        roster::accept(&store, e, 10).unwrap();
    }
    drop(store);
    let i = start(home.path(), &[], snapshot).await;
    let port = ok(&i, "status", json!({})).await["port"].as_u64().unwrap();
    let key = iroh::PublicKey::from_bytes(&key_bytes(&w.key).unwrap()).unwrap();
    let addr =
        iroh::EndpointAddr::new(key).with_ip_addr(format!("127.0.0.1:{port}").parse().unwrap());
    let mut out = vec![];
    for id in [ia, ib, ic] {
        let ep = endpoint(&id).await;
        let conn = ep.connect(addr.clone(), ALPN).await.unwrap();
        let (send, recv) = conn.open_bi().await.unwrap();
        let mut p = Peer { conn, send, recv };
        p.hello(vec![]).await;
        out.push((id, p, ep));
    }
    for _ in 0..250 {
        if ok(&i, "status", json!({})).await["peers"]["online"] == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(ok(&i, "status", json!({})).await["peers"]["online"], 3);
    (home, i, out.try_into().ok().unwrap())
}

fn envelope(id: &str, rid: &str, from: &str, to: &str) -> Frame {
    Frame::Envelope {
        id: id.into(),
        request_id: rid.into(),
        kind: "message".into(),
        from: from.into(),
        to: to.into(),
        reply_to: None,
        thread: None,
        hops: 0,
        body: "hi".into(),
        files: vec![],
        sent_at: 0,
    }
}

/// `p` sends an envelope from its `<short>/codex:<conv>` to `to`: the instance's answer.
async fn ask(p: &mut Peer, who: &Identity, rid: &str, to: &str) -> (String, Option<String>) {
    let id = ulid();
    p.send(&envelope(&id, rid, &format!("{}/codex:c1", who.short), to))
        .await;
    match p
        .recv_where(|f| matches!(f, Frame::Ack { id: i, .. } if *i == id))
        .await
    {
        Frame::Ack { state, reason, .. } => (state, reason),
        _ => unreachable!(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_own_workspace_computers_start_a_conversation() {
    let (home, i, [(ia, mut pa, _a), (ib, mut pb, _b), (ic, mut pc, _c)]) = seeded(None).await;
    declare(&i, &["friday"]).await;
    let friday = format!("{}/friday", me(&i).await);
    let policy = Some("policy".to_string());
    assert_eq!(ask(&mut pa, &ia, "a1", &friday).await.0, "stored");
    assert_eq!(
        ask(&mut pb, &ib, "b1", &friday).await,
        ("refused".into(), policy.clone())
    );
    assert_eq!(
        ask(&mut pc, &ic, "c1", &friday).await,
        ("refused".into(), policy.clone())
    );
    // Friday writes to C: C may answer for a day.
    let to = format!("{}/codex:c1", ic.short);
    ok(
        &i,
        "send",
        json!({"from": friday, "to": to, "text": "hello carol", "request_id": "f1"}),
    )
    .await;
    pc.recv_where(|f| matches!(f, Frame::Envelope { .. })).await;
    assert_eq!(ask(&mut pc, &ic, "c2", &friday).await.0, "stored");
    let db = rusqlite::Connection::open(home.path().join("state.db")).unwrap();
    db.execute(
        "UPDATE messages SET sent_at = sent_at - 90000 WHERE from_key = ?1",
        [&friday],
    )
    .unwrap();
    assert_eq!(
        ask(&mut pc, &ic, "c3", &friday).await,
        ("refused".into(), policy)
    );
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_and_events() {
    let out = tempfile::tempdir().unwrap();
    let last = out.path().join("last.db");
    let (home, i, [(ia, mut pa, _a), (ib, _pb, _b), _c]) = seeded(Some(&last)).await;
    let mut events = i.events();
    declare(&i, &["friday"]).await;
    let friday = format!("{}/friday", me(&i).await);
    let to = format!("{}/codex:a1", ia.short);
    ok(
        &i,
        "send",
        json!({"from": friday, "to": to, "text": "hi", "request_id": "f1"}),
    )
    .await;
    let id = match pa.recv_where(|f| matches!(f, Frame::Envelope { .. })).await {
        Frame::Envelope { id, .. } => id,
        _ => unreachable!(),
    };
    for (ack, state) in [(true, "stored"), (false, "delivered")] {
        let (id2, state2, reason) = (id.clone(), state.to_string(), None);
        let f = if ack {
            Frame::Ack {
                id: id2,
                state: state2,
                reason,
            }
        } else {
            Frame::Receipt {
                id: id2,
                state: state2,
                reason,
            }
        };
        pa.send(&f).await;
        let e = event(&mut events, |e| {
            e["event"] == "receipt_changed" && e["state"] == state
        })
        .await;
        assert_eq!(e["id"], id.as_str());
    }
    // The founder removes B: every subscriber hears of it.
    let store = Store::open(&home.path().join("state.db")).unwrap();
    let heads = roster::replay(&roster::entries(&store).unwrap()).heads;
    let rm = Entry::signed(&ia, heads, Kind::Remove, json!({"member": ib.key}), now_s());
    pa.send(&Frame::Roster {
        entries: vec![rm],
        more: false,
    })
    .await;
    let e = event(&mut events, |e| e["event"] == "member_removed").await;
    assert_eq!(e["member"], ib.key.as_str());

    let copy = out.path().join("copy.db");
    ok(&i, "snapshot", json!({"path": copy})).await;
    let snap = Store::open(&copy).unwrap();
    assert!(!roster::entries(&snap).unwrap().is_empty());
    i.shutdown().await;
    assert!(!roster::entries(&Store::open(&last).unwrap())
        .unwrap()
        .is_empty());
}
