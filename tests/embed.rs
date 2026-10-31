//! An embedded instance: the engine in process (no socket, launchd, hooks or dialog) as a
//! Lightbulb workspace's member, a member like any other, whose sessions are the workspace's
//! agents and whose `members` say whose computers they answer (by sign-in proof).

mod common;

use common::{call, dialog, endpoint, identity, now_s, ulid, until, Env, Peer};
use lightbulb::embed::Instance;
use lightbulb::identity::{key_bytes, Identity};
use lightbulb::net::frames::Frame;
use lightbulb::net::ALPN;
use lightbulb::roster::{self, Entry, Kind};
use lightbulb::store::Store;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use tokio::sync::broadcast::Receiver;

const NAME: &str = "The Lightbulb Company";

/// An instance on `home` that dials `envs` at their running daemons, with a final snapshot to
/// `snapshot` at shutdown.
async fn start(home: &Path, envs: &[&Env], snapshot: Option<&Path>) -> Instance {
    try_start(home, envs, snapshot).await.unwrap()
}

async fn try_start(
    home: &Path,
    envs: &[&Env],
    snapshot: Option<&Path>,
) -> lightbulb::Result<Instance> {
    common::instance(home, envs, NAME, snapshot, &[]).await
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

/// A genesis by `id` ("prasiddha" on "mac", team "acme").
fn genesis(id: &Identity) -> Entry {
    let body = json!({"member": id.key, "name": "prasiddha", "computer": "mac",
        "role": "founder", "ticket_id": "", "team_name": "acme"});
    Entry::signed(id, vec![], Kind::Admit, body, 1)
}

/// A running computer A, founder of its own network, whose dialogs answer Allow.
fn computer_a() -> Env {
    let a = Env::new();
    roster::accept(&a.store(), &genesis(&a.identity()), 10).unwrap();
    a.sign_in(&common::user_of(&a.short()), None);
    a.daemon(false, "", &[dialog("Allow")]);
    a
}

/// The instance joins A's network through a once ask link A minted (A allows it), as
/// `invite --agents` has it; A then lets the workspace's agents write without a hold.
async fn join(i: &Instance, a: &Env) -> Value {
    let t = a.ask(json!({"id": 1, "op": "team.invite", "mode": "once"}));
    let ticket = t["ticket"].as_str().unwrap();
    let r = ok(i, "team.join", json!({"ticket": ticket, "name": NAME})).await;
    r
}

/// Declares `names` (agent: the name uppercased), answering the people of the computers `keys`
/// (each signed in as `common::user_of` its short).
async fn declare(i: &Instance, names: &[&str], keys: &[&Identity]) {
    let people: Vec<String> = keys.iter().map(|k| common::user_of(&k.short)).collect();
    let s: Vec<Value> = names
        .iter()
        .map(|n| json!({"name": n, "agent": n.to_uppercase(), "title": format!("{n} agent")}))
        .collect();
    ok(
        i,
        "cloud.sessions.set",
        json!({ "sessions": s, "members": people }),
    )
    .await;
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
    declare(&i, &["friday"], &[&a.identity()]).await;
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
    let key = ok(&i, "identity.get", json!({})).await["member_key"].clone();
    assert_eq!(rec["person_id"], key);
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
    // A replayed send and the `receipt` op carry the same thread.
    let again = json!({"from": friday, "to": sa, "text": "hello from friday", "request_id": "f1"});
    let again = ok(&i, "send", again).await;
    let asked = ok(&i, "receipt", json!({"from": friday, "request_id": "f1"})).await;
    assert_eq!(
        again["receipt"]["thread"], r["receipt"]["thread"],
        "{again}"
    );
    assert_eq!(
        asked["receipt"]["thread"], r["receipt"]["thread"],
        "{asked}"
    );
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
        // `holds`, `hold.get` and `hold.answer` (with `decided_by`) are the bridge's.
        (
            "hold.file",
            json!({"kind": "permission", "session": "x", "prompt": "p"}),
        ),
        ("share.grant", json!({"session": "x", "to": x})),
    ] {
        refused(&call(&i, op, body).await, "not_a_person");
    }
    // A workspace member mints no link, unlinks nothing, and stops only by `shutdown()`.
    for (op, body) in [
        ("team.unlink", json!({})),
        ("team.invite", json!({})),
        ("team.invite", json!({"mode": "once"})),
        ("team.invite", json!({"mode": "open"})),
        ("shutdown", json!({})),
    ] {
        assert_eq!(call(&i, op, body).await["error"], "unsupported", "{op}");
    }
    // Its solo network is the workspace's.
    let me = ok(&i, "identity.get", json!({})).await;
    assert_eq!(
        (me["name"].as_str(), me["computer"].as_str()),
        (Some(NAME), Some(NAME))
    );
    // One engine per home; a snapshot never lands in it.
    assert!(try_start(home.path(), &[], None).await.is_err());
    for p in [
        home.path().join("state.db"),
        home.path().join("x.db"),
        "rel.db".into(),
    ] {
        let r = call(&i, "snapshot", json!({ "path": p })).await;
        assert_eq!(r["error"], "bad_request", "{p:?}: {r}");
    }
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_undeclared_session_cannot_send() {
    let home = tempfile::tempdir().unwrap();
    let i = start(home.path(), &[], None).await;
    declare(&i, &["friday"], &[]).await;
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
    assert_eq!(join(&i, &a).await["state"], "admitted");
    let again = join(&i, &a).await;
    assert_eq!(again["reason"], "already_in_team", "{again}");
    let me = ok(&i, "identity.get", json!({})).await;
    assert_eq!(me["person_id"], me["member_key"], "{me}");
    assert_eq!(
        call(&i, "team.invite", json!({})).await["error"],
        "unsupported"
    );
    let key = me["member_key"].clone();
    let team = ok(&i, "team.get", json!({})).await;
    let mine = team["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["key"] == key)
        .cloned()
        .unwrap();
    assert_eq!(
        (mine["person_id"].as_str(), mine["name"].as_str()),
        (key.as_str(), Some(NAME))
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

/// An instance whose stored team (before it starts) is A (founder), B, C and itself; returns
/// its home, it and the three, connected.
async fn seeded(snapshot: Option<&Path>) -> (tempfile::TempDir, Instance, [Played; 3]) {
    seeded_with("", snapshot).await
}

/// `seeded` with `config` as the instance's `config.toml`.
async fn seeded_with(
    config: &str,
    snapshot: Option<&Path>,
) -> (tempfile::TempDir, Instance, [Played; 3]) {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.toml"), config).unwrap();
    let w = Identity::load_or_create(home.path()).unwrap();
    let (ia, ib, ic) = (identity(), identity(), identity());
    let g = genesis(&ia);
    let ab = common::admit(&ia, &g, &ib, "bob", json!({}));
    let ac = common::admit(&ia, &ab, &ic, "carol", json!({}));
    let aw = common::admit(&ia, &ac, &w, NAME, json!({}));
    let store = Store::open(&home.path().join("state.db")).unwrap();
    for e in [&g, &ab, &ac, &aw] {
        roster::accept(&store, e, 10).unwrap();
    }
    drop(store);
    let i = start(home.path(), &[], snapshot).await;
    let mut out = vec![];
    for id in [ia, ib, ic] {
        let (p, ep) = common::dial_instance(&i, &id).await;
        out.push((id, p, ep));
    }
    common::online(&i, 3).await;
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
        meta: Default::default(),
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
async fn only_listed_computers_reach_its_sessions() {
    let (home, i, [(ia, mut pa, _a), (ib, mut pb, _b), (ic, mut pc, _c)]) = seeded(None).await;
    declare(&i, &["friday"], &[&ia]).await;
    let friday = format!("{}/friday", me(&i).await);
    let no = ("refused".to_string(), Some("not_a_member".to_string()));
    assert_eq!(ask(&mut pa, &ia, "a1", &friday).await.0, "stored");
    assert_eq!(ask(&mut pb, &ib, "b1", &friday).await, no);
    // Friday writing to C first gives C no way in either.
    let to = format!("{}/codex:c1", ic.short);
    ok(
        &i,
        "send",
        json!({"from": friday, "to": to, "text": "hello carol", "request_id": "f1"}),
    )
    .await;
    assert_eq!(ask(&mut pc, &ic, "c1", &friday).await, no);
    // Nothing held, nothing in the mailbox but A's.
    assert_eq!(ok(&i, "holds", json!({})).await["holds"], json!([]));
    let db = home.path().join("state.db");
    assert_eq!(common::count(&db, "SELECT COUNT(*) FROM holds"), 0);
    // C, listed once its person joins the workspace, gets in.
    declare(&i, &["friday"], &[&ia, &ic]).await;
    assert_eq!(ask(&mut pc, &ic, "c2", &friday).await.0, "stored");
    i.shutdown().await;
}

/// `p` sends an assignment from its `<short>/codex:c1` to `to`: the instance's answer.
async fn assign(p: &mut Peer, who: &Identity, rid: &str, to: &str) -> (String, Option<String>) {
    let id = ulid();
    p.send(&Frame::Envelope {
        id: id.clone(),
        request_id: rid.into(),
        kind: "assign".into(),
        from: format!("{}/codex:c1", who.short),
        to: to.into(),
        reply_to: None,
        thread: None,
        hops: 0,
        body: "fix it".into(),
        files: vec![],
        sent_at: 0,
        meta: lightbulb::mail::Meta {
            done_when: Some("green".into()),
            deadline_at: Some(now_s() + 300),
            ..Default::default()
        },
    })
    .await;
    match p
        .recv_where(|f| matches!(f, Frame::Ack { id: i, .. } if *i == id))
        .await
    {
        Frame::Ack { state, reason, .. } => (state, reason),
        _ => unreachable!(),
    }
}

/// A workspace agent takes an assignment only when the instance's own `config.toml` turns
/// `network_workers` on, through the same gate as any computer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_instance_refuses_assignments_until_its_config_turns_them_on() {
    let (_home, i, [(ia, mut pa, _a), _, _]) = seeded_with("", None).await;
    declare(&i, &["friday"], &[&ia]).await;
    let friday = format!("{}/friday", me(&i).await);
    assert_eq!(
        assign(&mut pa, &ia, "as1", &friday).await,
        ("refused".to_string(), Some("unsupported".to_string()))
    );
    let peek = ok(&i, "inbox.peek", json!({})).await;
    assert_eq!(peek["sessions"], json!([]), "{peek}");
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_instance_stores_an_assignment_for_a_declared_agent_when_on() {
    let (_home, i, [(ia, mut pa, _a), _, _]) = seeded_with("network_workers = true\n", None).await;
    declare(&i, &["friday"], &[&ia]).await;
    let friday = format!("{}/friday", me(&i).await);
    assert_eq!(assign(&mut pa, &ia, "as1", &friday).await.0, "stored");
    let peek = ok(&i, "inbox.peek", json!({})).await;
    let r = &peek["sessions"][0]["records"][0];
    assert_eq!(peek["sessions"][0]["session"], "friday", "{peek}");
    assert_eq!(r["kind"], "assign", "{r}");
    assert_eq!(r["done_when"], "green", "{r}");
    assert!(r["deadline_at"].as_i64().unwrap() > now_s(), "{r}");
    assert!(r["framed"].as_str().unwrap().contains("reply_to=\"as1\""));
    // Friday's reply with `reply_to` is the assignment's result at the asker.
    let to = format!("{}/codex:c1", ia.short);
    ok(
        &i,
        "send",
        json!({"from": friday, "to": to, "text": "done", "request_id": "re1", "reply_to": "as1"}),
    )
    .await;
    let f = pa
        .recv_where(|f| matches!(f, Frame::Envelope { request_id, .. } if request_id == "re1"))
        .await;
    let Frame::Envelope { kind, .. } = f else {
        unreachable!()
    };
    assert_eq!(kind, "result");
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_and_events() {
    let out = tempfile::tempdir().unwrap();
    let last = out.path().join("last.db");
    let (home, i, [(ia, mut pa, _a), (ib, _pb, _b), _c]) = seeded(Some(&last)).await;
    let mut events = i.events();
    declare(&i, &["friday"], &[&ia]).await;
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
                limit: None,
            }
        } else {
            Frame::Receipt {
                id: id2,
                state: state2,
                reason,
                limit: None,
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
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let snap = Store::open(&copy).unwrap();
    assert!(!roster::entries(&snap).unwrap().is_empty());
    i.shutdown().await;
    // The home is free again once the instance is down.
    try_start(home.path(), &[], None)
        .await
        .unwrap()
        .shutdown()
        .await;
    assert!(!roster::entries(&Store::open(&last).unwrap())
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_message_event_per_message_and_none_for_an_undeclared_name() {
    let (_home, i, [(ia, mut pa, _a), _b, _c]) = seeded(None).await;
    let mut events = i.events();
    declare(&i, &["friday"], &[&ia]).await;
    let w = me(&i).await;
    let friday = format!("{w}/friday");
    let from = format!("{}/codex:c1", ia.short);
    // Mail for a name the instance did not declare is refused as unadvertised.
    let ghost = ulid();
    pa.send(&envelope(&ghost, "g1", &from, &format!("{w}/ghost")))
        .await;
    let ack = |id: String| move |f: &Frame| matches!(f, Frame::Ack { id: i, .. } if *i == id);
    match pa.recv_where(ack(ghost)).await {
        Frame::Ack { state, reason, .. } => {
            assert_eq!(
                (state.as_str(), reason.as_deref()),
                ("refused", Some("policy"))
            )
        }
        _ => unreachable!(),
    }
    // The same envelope twice (a lost ack), then another: two events, one for each message.
    let (first, second) = (ulid(), ulid());
    for (id, rid) in [(&first, "a1"), (&first, "a1"), (&second, "a2")] {
        pa.send(&envelope(id, rid, &from, &friday)).await;
        pa.recv_where(ack(id.clone())).await;
    }
    let e1 = event(&mut events, |e| e["event"] == "message").await;
    let e2 = event(&mut events, |e| e["event"] == "message").await;
    assert_eq!(
        (e1["id"].as_str(), e2["id"].as_str()),
        (Some(first.as_str()), Some(second.as_str()))
    );
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_instance_shuts_down_before_its_home_is_free() {
    let home = tempfile::tempdir().unwrap();
    let i = start(home.path(), &[], None).await;
    let key = ok(&i, "identity.get", json!({})).await["member_key"]
        .as_str()
        .unwrap()
        .to_string();
    let port = ok(&i, "status", json!({})).await["port"].as_u64().unwrap();
    drop(i); // no shutdown: a panic or a cancelled future
    let mut next = None;
    for _ in 0..500 {
        if let Ok(n) = try_start(home.path(), &[], None).await {
            next = Some(n);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let next = next.expect("the home is free once the dropped instance is down");
    // By then the old endpoint is gone: nothing answers at its address.
    let pk = iroh::PublicKey::from_bytes(&key_bytes(&key).unwrap()).unwrap();
    let old =
        iroh::EndpointAddr::new(pk).with_ip_addr(format!("127.0.0.1:{port}").parse().unwrap());
    let ep = endpoint(&identity()).await;
    let dial = tokio::time::timeout(Duration::from_secs(2), ep.connect(old, ALPN)).await;
    assert!(!matches!(dial, Ok(Ok(_))), "the old endpoint still answers");
    // A snapshot path inside the home is refused at start, too.
    next.shutdown().await;
    let inside = home.path().join("final.db");
    let r = try_start(home.path(), &[], Some(&inside)).await;
    assert!(r.err().is_some_and(|e| e.to_string().contains("snapshot")));
}

/// The prod path, end to end: an admin's computer A mints a once ask link (`invite --agents`)
/// and the workspace's instance joins with it, A allowing it; with A's person in `members`,
/// A writes to `<instance>/friday` (in the instance's inbox) and Friday's reply reaches A.
/// Computer B, admitted through A's own link, sees Friday too, but its key is not listed: its
/// message is refused `not_a_member`, with no hold on the instance.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admins_once_link_brings_the_workspace_in_and_only_members_reach_friday() {
    let a = computer_a();
    let home = tempfile::tempdir().unwrap();
    let i = start(home.path(), &[&a], None).await;
    assert_eq!(join(&i, &a).await["state"], "admitted");
    declare(&i, &["friday"], &[&a.identity()]).await;
    let friday = format!("{}/friday", me(&i).await);
    until(10, "A sees friday", || common::sees(&a, &friday));

    let sa = common::session(&a, "a1");
    a.send(&sa, &friday, "hi friday", "a1", None);
    until(10, "A's message is stored", || {
        a.state(&sa, "a1") == "stored"
    });
    let got = ok(&i, "inbox", json!({"session": "friday"})).await;
    let msgs = got["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1, "{got}");
    assert_eq!(msgs[0]["text"], "hi friday");
    let r = ok(
        &i,
        "send",
        json!({"from": "friday", "text": "hello A", "request_id": "f1", "reply_to": "a1"}),
    )
    .await;
    assert!(r["receipt"].is_object(), "{r}");
    until(10, "Friday's reply is in A's mailbox", || {
        a.unread(&sa) == 1
    });

    let key = ok(&i, "identity.get", json!({})).await["member_key"].clone();
    let port = ok(&i, "status", json!({})).await["port"].clone();
    let peers = format!(
        "{},{}@127.0.0.1:{port}",
        a.peer_arg(),
        key.as_str().unwrap()
    );
    let ask = common::invite(&a, "once");
    let b = Env::new();
    b.daemon(false, "", &[("LIGHTBULB_CONNECT_PEERS", peers)]);
    let out = b.detached(&[common::EXE, "join", &ask, "--name", "B"], "");
    assert!(
        out.status.success(),
        "{}{}",
        common::out(&out),
        common::err(&out)
    );
    until(10, "B sees friday", || common::sees(&b, &friday));
    let sb = common::session(&b, "b1");
    b.send(&sb, &friday, "hi from b", "b1", None);
    until(10, "B's message is refused", || {
        b.state(&sb, "b1") == "refused"
    });
    assert_eq!(b.receipt(&sb, "b1")["reason"], "not_a_member");
    let holds = ok(&i, "holds", json!({})).await;
    assert_eq!(holds["holds"], json!([]), "{holds}");
    let got = ok(&i, "inbox", json!({"session": "friday"})).await;
    assert_eq!(got["messages"], json!([]), "{got}");
    i.shutdown().await;
}

/// A connected computer's sign-in change reaches the workspace at once, with no restart:
/// after `lightbulb logout` its next message is refused `not_a_member`; signed in again (the
/// daemon told `login.changed`, as `lightbulb login` does), it is stored again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_logout_and_a_new_login_reach_a_connected_workspace_without_a_restart() {
    let a = computer_a();
    let home = tempfile::tempdir().unwrap();
    let i = start(home.path(), &[&a], None).await;
    assert_eq!(join(&i, &a).await["state"], "admitted");
    declare(&i, &["friday"], &[&a.identity()]).await;
    let friday = format!("{}/friday", me(&i).await);
    until(10, "A sees friday", || common::sees(&a, &friday));
    let sa = common::session(&a, "a1");
    a.send(&sa, &friday, "signed in", "r1", None);
    until(10, "stored", || a.state(&sa, "r1") == "stored");

    let out = a.detached(&[common::EXE, "logout"], "");
    assert!(out.status.success(), "{}", common::err(&out));
    a.send(&sa, &friday, "signed out", "r2", None);
    until(10, "refused after logout", || {
        a.state(&sa, "r2") == "refused"
    });
    assert_eq!(a.receipt(&sa, "r2")["reason"], "not_a_member");

    a.sign_in(&common::user_of(&a.short()), None);
    assert_eq!(a.ask(json!({"id": 1, "op": "login.changed"}))["ok"], true);
    a.send(&sa, &friday, "signed in again", "r3", None);
    until(10, "stored after login", || a.state(&sa, "r3") == "stored");
    i.shutdown().await;
}

fn imported(subject: &str, fact: &str) -> Value {
    json!({"subject": subject, "fact": fact, "kind": "fact", "scope": "team", "when": 5,
        "source": {"harness": "import", "repo": null, "session": null, "on_behalf_of": "Maya"},
        "replaces": []})
}

async fn listed(i: &Instance, since: u64) -> Value {
    ok(i, "memory.list", json!({ "since_seq": since })).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cloud_remember_signs_as_workspace_with_name_in_source() {
    let home = tempfile::tempdir().unwrap();
    let i = start(home.path(), &[], None).await;
    let key = ok(&i, "identity.get", json!({})).await["member_key"].clone();
    let r = ok(
        &i,
        "memory.remember",
        json!({"subject": "db", "fact": "we use postgres", "kind": "decision",
            "scope": "team", "on_behalf_of": "Maya"}),
    )
    .await;
    assert_eq!(r["stored"], true, "{r}");
    let l = listed(&i, 0).await;
    let e = &l["entries"][0];
    assert_eq!(e["id"], r["entry_id"], "{l}");
    assert_eq!(e["author"], key);
    assert_eq!(e["op"]["source"]["harness"], "workspace");
    assert_eq!(e["op"]["source"]["on_behalf_of"], "Maya");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cloud_import_assigns_sequential_seq() {
    let home = tempfile::tempdir().unwrap();
    let i = start(home.path(), &[], None).await;
    let r = ok(
        &i,
        "memory.import",
        json!({"entries": [imported("a", "one"), imported("b", "two"), imported("c", "three")]}),
    )
    .await;
    let ids = r["ids"].as_array().unwrap();
    assert_eq!(ids.len(), 3, "{r}");
    let l = listed(&i, 0).await;
    let got: Vec<(&Value, &Value)> = l["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (&e["seq"], &e["id"]))
        .collect();
    assert_eq!(
        got,
        vec![
            (&json!(1), &ids[0]),
            (&json!(2), &ids[1]),
            (&json!(3), &ids[2])
        ]
    );
    assert_eq!(l["entries"][0]["op"]["source"]["harness"], "import");
    assert_eq!(l["next"], 3);
    let bad = call(&i, "memory.import", json!({"entries": [{"subject": "x"}]})).await;
    assert_eq!(bad["error"], "bad_request", "{bad}");
    // A blank subject is refused, the rest go on.
    let r = ok(
        &i,
        "memory.import",
        json!({"entries": [imported(" ", "blank"), imported("d", "four")]}),
    )
    .await;
    assert_eq!(r["ids"][0], Value::Null, "{r}");
    assert!(r["ids"][1].is_string(), "{r}");
    assert_eq!(r["refused"], json!([{"index": 0, "reason": "no_subject"}]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cloud_list_since_returns_new_entries() {
    let home = tempfile::tempdir().unwrap();
    let i = start(home.path(), &[], None).await;
    ok(
        &i,
        "memory.import",
        json!({"entries": [imported("a", "one"), imported("b", "two")]}),
    )
    .await;
    let first = listed(&i, 0).await;
    let next = first["next"].as_u64().unwrap();
    assert_eq!(listed(&i, next).await["entries"], json!([]));
    assert_eq!(listed(&i, next).await["next"], next);
    ok(
        &i,
        "memory.import",
        json!({"entries": [imported("c", "three")]}),
    )
    .await;
    let l = listed(&i, next).await;
    assert_eq!(l["entries"].as_array().unwrap().len(), 1, "{l}");
    assert_eq!(l["entries"][0]["op"]["fact"], "three");
    assert_eq!(l["next"], next + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cloud_forget_author_tombstones_all() {
    let home = tempfile::tempdir().unwrap();
    let i = start(home.path(), &[], None).await;
    let key = ok(&i, "identity.get", json!({})).await["member_key"].clone();
    ok(
        &i,
        "memory.import",
        json!({"entries": [imported("a", "one"), imported("b", "two")]}),
    )
    .await;
    let r = ok(&i, "memory.forget_author", json!({"author": key})).await;
    assert_eq!(r["forgotten"], 2, "{r}");
    let facts = ok(&i, "memory.recall", json!({"question": ""})).await;
    assert_eq!(facts["facts"], json!([]), "{facts}");
    let again = ok(&i, "memory.forget_author", json!({"author": key})).await;
    assert_eq!(again["forgotten"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cloud_list_includes_tombstones_bodyless() {
    let home = tempfile::tempdir().unwrap();
    let i = start(home.path(), &[], None).await;
    let key = ok(&i, "identity.get", json!({})).await["member_key"].clone();
    let r = ok(
        &i,
        "memory.import",
        json!({"entries": [imported("a", "one")]}),
    )
    .await;
    ok(&i, "memory.forget_author", json!({"author": key})).await;
    let l = listed(&i, 0).await;
    assert_eq!(
        l["entries"][0],
        json!({"seq": 1, "id": r["ids"][0], "author": key, "tombstone": true}),
        "{l}"
    );
    // The forget itself is returned as stored, naming its target.
    assert_eq!(l["entries"][1]["op"]["target"], r["ids"][0], "{l}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cloud_ops_are_refused_on_a_socket_daemon() {
    let a = computer_a();
    for (op, body) in [
        ("memory.import", json!({"entries": []})),
        ("memory.forget_author", json!({"author": "x"})),
        ("memory.list", json!({"since_seq": 0})),
    ] {
        let mut req = body;
        req["id"] = 1.into();
        req["op"] = op.into();
        let r = a.ask(req);
        assert_eq!(
            (r["ok"].clone(), r["error"].clone()),
            (json!(false), json!("unsupported")),
            "{op}: {r}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cloud_import_is_idempotent_on_key() {
    let home = tempfile::tempdir().unwrap();
    let i = start(home.path(), &[], None).await;
    let mut a = imported("a", "one");
    a["key"] = "rec1:h1".into();
    let mut b = imported("b", "two");
    b["key"] = "rec2:h2".into();
    let batch = json!({"entries": [a, b, imported("c", "three")]});
    let first = ok(&i, "memory.import", batch.clone()).await;
    let again = ok(&i, "memory.import", batch).await;
    assert_eq!(
        first["ids"].as_array().unwrap()[..2],
        again["ids"].as_array().unwrap()[..2],
        "{first} {again}"
    );
    // No key: a second entry, as before.
    assert_ne!(first["ids"][2], again["ids"][2]);
    let l = listed(&i, 0).await;
    assert_eq!(l["entries"].as_array().unwrap().len(), 4, "{l}");
    assert_eq!(l["entries"][0]["op"]["source"]["session"], "rec1:h1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cloud_import_continues_past_a_refused_entry() {
    let home = tempfile::tempdir().unwrap();
    let i = start(home.path(), &[], None).await;
    let r = ok(
        &i,
        "memory.import",
        json!({"entries": [imported("a", "one"),
            imported("b", "key is sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789"),
            imported("c", "three")]}),
    )
    .await;
    assert!(
        r["ids"][0].is_string() && r["ids"][1].is_null() && r["ids"][2].is_string(),
        "{r}"
    );
    assert_eq!(r["refused"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["refused"][0]["index"], 1);
    assert!(
        r["refused"][0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("secret:"),
        "{r}"
    );
    assert_eq!(listed(&i, 0).await["entries"].as_array().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cloud_import_key_of_forgotten_entry_returns_the_old_id() {
    let home = tempfile::tempdir().unwrap();
    let i = start(home.path(), &[], None).await;
    let key = ok(&i, "identity.get", json!({})).await["member_key"].clone();
    let mut a = imported("a", "one");
    a["key"] = "rec1:h1".into();
    let batch = json!({"entries": [a]});
    let first = ok(&i, "memory.import", batch.clone()).await;
    ok(&i, "memory.forget_author", json!({"author": key})).await;
    let before = listed(&i, 0).await["entries"].as_array().unwrap().len();
    let again = ok(&i, "memory.import", batch).await;
    assert_eq!(again["ids"], first["ids"], "{again}");
    assert_eq!(
        listed(&i, 0).await["entries"].as_array().unwrap().len(),
        before
    );
}

/// `memory.share` takes the team lock before the store's, as everything else does: with another
/// thread locking team then store in a loop, 100 shares finish (the opposite order would
/// deadlock, ABBA).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_share_takes_the_team_lock_before_the_store() {
    let home = tempfile::tempdir().unwrap();
    let i = std::sync::Arc::new(start(home.path(), &[], None).await);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let other = {
        let (i, stop) = (i.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                i.lock_team_then_store();
            }
        })
    };
    let shares = async {
        for _ in 0..100 {
            let r = call(&i, "memory.share", json!({"entry": "abcdef"})).await;
            assert_eq!(r["error"], "not_found", "{r}");
        }
    };
    tokio::time::timeout(Duration::from_secs(20), shares)
        .await
        .expect("100 shares finish beside a team-then-store locker");
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    other.join().unwrap();
}

/// `f` again while A's running daemon holds its database (`database is locked`).
fn busy_retry<T>(mut f: impl FnMut() -> lightbulb::Result<T>) -> T {
    for _ in 0..50 {
        if let Ok(v) = f() {
            return v;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    f().unwrap()
}

/// A wipe tombstones entries of another author too, and a replica honours the forget.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cloud_forget_author_wipes_a_peers_entry_and_a_replica_honours_it() {
    let a = computer_a();
    let home = tempfile::tempdir().unwrap();
    let i = start(home.path(), &[&a], None).await;
    join(&i, &a).await;
    let ka = a.identity().key;
    let theirs = busy_retry(|| {
        lightbulb::memory::remember(
            &a.store(),
            &a.identity(),
            lightbulb::memory::Op::Remember {
                subject: "db".into(),
                fact: "postgres 16".into(),
                kind: lightbulb::memory::Kind::Fact,
                scope: lightbulb::memory::Scope::Team,
                when: None,
                source: lightbulb::memory::Source {
                    harness: "test".into(),
                    repo: None,
                    session: None,
                    on_behalf_of: None,
                    shared_from: None,
                },
                replaces: vec![],
            },
            5,
        )
    });
    for _ in 0..100 {
        let l = listed(&i, 0).await;
        if l["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["id"] == theirs.id.as_str())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let r = ok(&i, "memory.forget_author", json!({"author": ka})).await;
    assert_eq!(r["forgotten"], 1, "{r}");
    let l = listed(&i, 0).await;
    assert!(
        l["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["id"] == theirs.id.as_str() && e["tombstone"] == true),
        "{l}"
    );
    let facts = ok(&i, "memory.recall", json!({"question": ""})).await;
    assert_eq!(facts["facts"], json!([]), "{facts}");
    // The forget is signed by the workspace member; A, the author, honours it.
    let me_key = ok(&i, "identity.get", json!({})).await["member_key"]
        .as_str()
        .unwrap()
        .to_string();
    until(10, "A holds the workspace's forget", || {
        !lightbulb::memory::since(&a.store(), &me_key, 0, 10)
            .unwrap()
            .is_empty()
    });
    for f in lightbulb::memory::since(&a.store(), &me_key, 0, 10).unwrap() {
        busy_retry(|| lightbulb::memory::accept(&a.store(), &f, 10));
    }
    assert!(lightbulb::memory::entry(&a.store(), &theirs.id)
        .unwrap()
        .is_none());
}
