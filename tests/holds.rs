//! Holds: another person's message parked until its person allows it, the answer doors, and
//! expiry. The daemon tests play the teammate (`Peer`); the expiry tests drive `holds` with
//! their own clock.

use lightbulb_connect::daemon::net_ops;
use lightbulb_connect::holds::{self, Scope};
use lightbulb_connect::identity::Identity;
use lightbulb_connect::mail::{self, State};
use lightbulb_connect::net::frames::Frame;
use lightbulb_connect::roster::{self, Entry, Kind, Roster};
use lightbulb_connect::sessions::Key;
use lightbulb_connect::store::{Session, Store};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

mod common;

use common::{dialog, endpoint, person, seed_team, ulid, until, Env, Peer};

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
    async fn ack(&mut self, id: &str) -> (String, Option<String>) {
        match self
            .recv_where(|f| matches!(f, Frame::Ack { id: i, .. } if i == id))
            .await
        {
            Frame::Ack { state, reason, .. } => (state, reason),
            _ => unreachable!(),
        }
    }
}

/// A running alone (dialogs answered `button`), its teammate B (another person, not yet
/// allowed) played by the test.
async fn a_and_teammate(button: &str) -> (Env, Env, iroh::Endpoint, Peer) {
    a_and_teammate_with(button, None).await
}

/// `a_and_teammate`, A's daemon also given `pin` (`LIGHTBULB_CONNECT_HOOK_CALLER`).
async fn a_and_teammate_with(
    button: &str,
    pin: Option<(&'static str, String)>,
) -> (Env, Env, iroh::Endpoint, Peer) {
    let (a, b) = (Env::new(), Env::new());
    seed_team(&a, &b);
    let mut env = vec![dialog(button)];
    env.extend(pin);
    a.daemon(false, "", &env);
    let ep = endpoint(&b.identity()).await;
    let mut p = Peer::dial(&ep, &a).await;
    p.hello(vec![]).await;
    until(5, "B online", || a.status()["peers"]["online"] == 1);
    (a, b, ep, p)
}

fn session(e: &Env, conv: &str) -> String {
    format!("{}/codex:{conv}", e.short())
}

/// `e`'s open holds.
fn held(e: &Env) -> Vec<Value> {
    e.ask(json!({"id": 1, "op": "holds"}))["holds"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// Waits for one open hold on `e`: its id.
fn one_hold(e: &Env) -> String {
    let mut h = vec![];
    until(10, "a hold", || {
        h = held(e);
        !h.is_empty()
    });
    assert_eq!(h.len(), 1, "{h:?}");
    h[0]["id"].as_str().unwrap().to_string()
}

fn answer(e: &Env, id: &str, verdict: &str, scope: Option<&str>) -> Value {
    person(
        e,
        json!({"id": 1, "op": "hold.answer", "hold": id, "verdict": verdict, "scope": scope}),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn teammate_first_message_is_held_then_allow_delivers_and_writes_accept() {
    let (a, b, _ep, mut p) = a_and_teammate("").await;
    let (sb, sa) = (session(&b, "b1"), session(&a, "a1"));
    let id = ulid();
    p.send(&envelope(&id, "r1", &sb, &sa, "please review"))
        .await;
    assert_eq!(p.ack(&id).await, ("held".into(), None));
    let h = one_hold(&a);
    assert_eq!(h, "H1");
    let listed = &held(&a)[0];
    assert_eq!(listed["target_session"], sa);
    assert_eq!(listed["from"], sb);
    assert_eq!(listed["preview"], "please review");
    assert!(
        listed["label"].as_str().unwrap().contains("bob"),
        "{listed}"
    );
    // Never in the mailbox before Allow.
    assert_eq!(a.unread(&sa), 0);
    // A resend makes no second hold.
    p.send(&envelope(&id, "r1", &sb, &sa, "please review"))
        .await;
    assert_eq!(p.ack(&id).await.0, "held");
    assert_eq!(held(&a).len(), 1);

    let r = answer(&a, &h, "allow", None);
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["scope"], "person");
    p.recv_where(
        |f| matches!(f, Frame::Receipt { id: i, state, .. } if *i == id && state == "stored"),
    )
    .await;
    assert_eq!(a.take_one(&sa)["text"], "please review");
    p.recv_where(
        |f| matches!(f, Frame::Receipt { id: i, state, .. } if *i == id && state == "delivered"),
    )
    .await;
    let pol = a.ask(json!({"id": 1, "op": "policy.get"}));
    assert_eq!(pol["people"][b.identity().key], "accept", "{pol}");
    // The person is allowed now: another of B's sessions goes straight through.
    let id2 = ulid();
    p.send(&envelope(&id2, "r2", &session(&b, "b2"), &sa, "more"))
        .await;
    assert_eq!(p.ack(&id2).await.0, "stored");
    assert!(held(&a).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decline_refuses_and_delivers_nothing() {
    // The dialog answers Decline.
    let (a, b, _ep, mut p) = a_and_teammate("Decline").await;
    let (sb, sa) = (session(&b, "b1"), session(&a, "a1"));
    let id = ulid();
    p.send(&envelope(&id, "r1", &sb, &sa, "x")).await;
    assert_eq!(p.ack(&id).await.0, "held");
    p.recv_where(|f| matches!(f, Frame::Receipt { id: i, state, reason } if *i == id && state == "refused" && reason.as_deref() == Some("declined")))
        .await;
    assert!(held(&a).is_empty());
    assert_eq!(a.unread(&sa), 0);
    assert!(a.inbox(&sa).is_empty());
    // A resend is re-acked with its answer, not held again.
    p.send(&envelope(&id, "r1", &sb, &sa, "x")).await;
    assert_eq!(
        p.ack(&id).await,
        ("refused".into(), Some("declined".into()))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_answer_is_not_open() {
    let (a, b, _ep, mut p) = a_and_teammate("").await;
    let id = ulid();
    p.send(&envelope(
        &id,
        "r1",
        &session(&b, "b1"),
        &session(&a, "a1"),
        "x",
    ))
    .await;
    p.ack(&id).await;
    let h = one_hold(&a);
    assert_eq!(answer(&a, &h, "decline", None)["ok"], true);
    let r = answer(&a, &h, "allow", None);
    assert_eq!(
        (&r["ok"], &r["error"]),
        (&json!(false), &json!("not_open")),
        "{r}"
    );
    let r = answer(&a, "H99", "allow", None);
    assert_eq!(r["error"], "not_found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn always_on_a_computer_sender_is_bad_request() {
    let (a, b, _ep, mut p) = a_and_teammate("").await;
    let id = ulid();
    p.send(&envelope(
        &id,
        "r1",
        &session(&b, "b1"),
        &session(&a, "a1"),
        "x",
    ))
    .await;
    p.ack(&id).await;
    let h = one_hold(&a);
    for scope in ["cloud_of", "own_cloud"] {
        let r = answer(&a, &h, "allow", Some(scope));
        assert_eq!(r["error"], "bad_request", "{scope}: {r}");
    }
    assert_eq!(held(&a).len(), 1, "still open");
    // The CLI says so in words.
    let o = a.detached(&[common::EXE, "approve", &h, "--always"], "");
    assert_ne!(o.status.code(), Some(0));
    assert!(common::err(&o).contains("cloud"), "{}", common::err(&o));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approve_from_agent_shell_is_refused() {
    // A's daemon takes this test process for an agent; `person` stays a person.
    let me = std::process::id();
    let birth = lightbulb_connect::os::process(me).unwrap().birth_micros;
    let pin = ("LIGHTBULB_CONNECT_HOOK_CALLER", format!("{me}:{birth}"));
    let (a, b, _ep, mut p) = a_and_teammate_with("", Some(pin)).await;
    let id = ulid();
    p.send(&envelope(
        &id,
        "r1",
        &session(&b, "b1"),
        &session(&a, "a1"),
        "x",
    ))
    .await;
    p.ack(&id).await;
    let h = one_hold(&a);
    for op in [
        json!({"id": 1, "op": "hold.answer", "hold": h, "verdict": "allow"}),
        json!({"id": 1, "op": "hold.get", "hold": h}),
        json!({"id": 1, "op": "policy.set", "agents_send_without_asking": true}),
    ] {
        let r = a.ask(op);
        assert_eq!(
            (&r["error"], &r["reason"]),
            (&json!("bad_request"), &json!("not_a_person")),
            "{r}"
        );
    }
    assert_eq!(held(&a).len(), 1, "still open");
    assert_eq!(answer(&a, &h, "allow", None)["ok"], true, "a person may");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permission_hold_round_trip() {
    let a = Env::new();
    a.daemon(false, "", &[dialog("")]);
    // A registered session: this test process stands in for its agent.
    let me = lightbulb_connect::os::process(std::process::id()).unwrap();
    let r = a.ask(
        json!({"id": 1, "op": "register", "session": {"provider": "claude",
        "conversation": "c1", "pid": me.pid, "birth_micros": me.birth_micros, "cwd": "/tmp/app"}}),
    );
    let key = r["key"].as_str().unwrap().to_string();
    let mut events = UnixStream::connect(a.state.path().join("sock")).unwrap();
    writeln!(events, r#"{{"id":1,"op":"subscribe"}}"#).unwrap();
    events
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut events = BufReader::new(events);
    let mut line = String::new();
    events.read_line(&mut line).unwrap(); // the subscribe answer

    let r = person(
        &a,
        json!({"id": 1, "op": "hold.file", "kind": "permission", "session": key,
            "prompt": "Run `rm -rf build`?"}),
    );
    assert_eq!(r["ok"], true, "{r}");
    let h = r["hold"].as_str().unwrap().to_string();
    let listed = held(&a);
    assert_eq!(listed[0]["kind"], "permission");
    assert_eq!(listed[0]["preview"], "Run `rm -rf build`?");
    let got = person(&a, json!({"id": 1, "op": "hold.get", "hold": h}));
    assert_eq!(got["target_birth_micros"], me.birth_micros, "{got}");
    assert_eq!(answer(&a, &h, "allow", None)["ok"], true);
    let mut closed = Value::Null;
    while closed.is_null() {
        line.clear();
        events.read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        if v["event"] == "hold_closed" {
            closed = v;
        }
    }
    assert_eq!(
        closed,
        json!({"event": "hold_closed", "id": h, "by": "local", "verdict": "allow"})
    );

    // Unanswered for 600 s: expired.
    let (_d, s, ..) = lib();
    let k = "me/claude:c1";
    s.put_session(&row(k)).unwrap();
    let p = holds::file_permission(&s, k, "ok?", 1_000).unwrap();
    assert!(holds::expire(&s, 1_599).unwrap().is_empty());
    assert_eq!(holds::expire(&s, 1_600).unwrap(), vec![p.id]);
    assert!(holds::open(&s).unwrap().is_empty());
}

/// A store and roster of this computer (`me`, short `me`… as its key) and teammate `aden`
/// (another person), without a daemon.
fn lib() -> (tempfile::TempDir, Store, Roster, Identity, Identity) {
    let d = tempfile::tempdir().unwrap();
    let store = Store::open(&d.path().join("state.db")).unwrap();
    let (me, aden) = (common::identity(), common::identity());
    let g = net_ops::genesis(&me, "prasiddha", "mac", "acme", 1);
    let admit = Entry::signed(
        &me,
        vec![g.hash()],
        Kind::Admit,
        json!({"member": aden.key, "name": "aden", "computer": "amac", "person_id": aden.key,
               "kind": "computer", "role": "member"}),
        2,
    );
    let roster = roster::replay(&[g, admit]);
    (d, store, roster, me, aden)
}

fn row(key: &str) -> Session {
    Session {
        key: key.into(),
        provider: "codex".into(),
        conversation: None,
        pid: Some(1),
        birth_micros: Some(1),
        cwd: "/tmp/app".into(),
        title: None,
        status: "idle".into(),
        seen_at: 0,
        executable: None,
    }
}

/// Aden's message to `to` arrives at `now`: held. Its hold.
fn held_lib(
    s: &Store,
    roster: &Roster,
    me: &Identity,
    aden: &Identity,
    to: &str,
    now: u64,
) -> holds::Hold {
    let peer = &roster.members[&aden.key];
    let env = envelope(
        &ulid(),
        "r1",
        &format!("{}/codex:a", aden.short),
        to,
        "secret",
    );
    let got = mail::receive(s, &env, peer, roster, &me.short, now).unwrap();
    assert_eq!(got, (State::Held, None));
    holds::open(s).unwrap().remove(0)
}

fn receipt(s: &Store, aden: &Identity) -> mail::Receipt {
    let from = Key(format!("{}/codex:a", aden.short));
    mail::receipt(s, &from, "r1").unwrap().unwrap()
}

#[tokio::test]
async fn hold_expires_after_7_days() {
    let (_d, s, roster, me, aden) = lib();
    let to = format!("{}/codex:t", me.short);
    s.put_session(&row(&to)).unwrap();
    let h = held_lib(&s, &roster, &me, &aden, &to, 1_000);
    let seven = 1_000 + mail::HELD_S as i64;
    assert!(holds::expire(&s, seven - 1).unwrap().is_empty());
    assert_eq!(holds::expire(&s, seven).unwrap(), vec![h.id]);
    assert_eq!(receipt(&s, &aden).state, State::Expired);
    let r = holds::answer(&s, &roster, &me, h.id, true, None, "local", seven + 1).unwrap();
    assert_eq!(r.err(), Some("not_open"));
    assert!(mail::waiting(&s, &Key(to), seven as u64 + 1)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn hold_expires_24h_after_its_target_session_ends() {
    let (_d, s, roster, me, aden) = lib();
    let to = format!("{}/codex:t", me.short);
    s.put_session(&row(&to)).unwrap();
    let h = held_lib(&s, &roster, &me, &aden, &to, 1_000);
    s.end_session(&to, 5_000).unwrap();
    assert!(holds::expire(&s, 5_000 + 86_399).unwrap().is_empty());
    assert_eq!(holds::expire(&s, 5_000 + 86_400).unwrap(), vec![h.id]);
    let r = receipt(&s, &aden);
    assert_eq!(
        (r.state, r.reason.as_deref()),
        (State::Expired, Some("session_ended"))
    );
    let late = holds::answer(
        &s,
        &roster,
        &me,
        h.id,
        true,
        Some(Scope::Once),
        "local",
        100_000,
    );
    assert_eq!(late.unwrap().err(), Some("not_open"));
    assert!(mail::waiting(&s, &Key(to), 100_000).unwrap().is_empty());
    // A session that never had a row here is gone from the hold's start.
    let absent = format!("{}/codex:none", me.short);
    let env = envelope(
        &ulid(),
        "r2",
        &format!("{}/codex:a", aden.short),
        &absent,
        "x",
    );
    let peer = &roster.members[&aden.key];
    mail::receive(&s, &env, peer, &roster, &me.short, 200_000).unwrap();
    assert!(holds::expire(&s, 200_000 + 86_399).unwrap().is_empty());
    assert_eq!(holds::expire(&s, 200_000 + 86_400).unwrap().len(), 1);
}
