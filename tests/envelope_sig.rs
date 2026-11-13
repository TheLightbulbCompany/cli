//! Envelopes and the sending computer's signature: the receiver verifies `sig` over the raw
//! JSON it got and keeps `sig` and the signed `sent_at`; one carrying an older build's A2A
//! Message (`a2a`) is refused `unsupported`. A real daemon (A) and B played by the test (`Peer`).

use lightbulb::identity::Identity;
use lightbulb::net::frames::{envelope_sig, Frame};
use serde_json::{json, Value};

mod common;

use common::{endpoint, seed_pair, session, ulid, until};
use common::{Env, Peer};

const SIGNED_AT: i64 = 1_700_000_000;

/// A `message` envelope `id` from `from` to `to` with `body`, `a2a` and no `sig`, as JSON.
fn envelope(id: &str, from: &str, to: &str, body: &str, a2a: Option<Value>) -> Value {
    let f = Frame::Envelope {
        id: id.into(),
        request_id: format!("r-{id}"),
        kind: "message".into(),
        from: from.into(),
        to: to.into(),
        reply_to: None,
        thread: None,
        hops: 0,
        body: body.into(),
        files: vec![],
        sent_at: SIGNED_AT,
        meta: Default::default(),
        a2a,
        sig: None,
    };
    serde_json::to_value(&f).unwrap()
}

/// `v` with its `sig` by `who`.
fn signed(who: &Identity, mut v: Value) -> Value {
    v["sig"] = envelope_sig(who, &v).into();
    v
}

fn user(text: &str) -> Value {
    json!({"role": "ROLE_USER", "parts": [{"text": text}]})
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

fn refused(reason: &str) -> (String, Option<String>) {
    ("refused".into(), Some(reason.into()))
}

/// A member's plain message is delivered: membership is the gate, nothing holds it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signed_member_message_is_stored_with_sig_and_signed_at_and_no_hold() {
    let (a, b, _ep, mut p) = a_and_peer_b().await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let v = signed(&b.identity(), envelope(&id, &sb, &sa, "hi", None));
    p.send_raw(&v).await;
    assert_eq!(p.ack(&id).await.0, "stored");
    let m = a.store().message(&id).unwrap().expect("stored");
    assert_eq!(m.sig.as_deref(), v["sig"].as_str());
    assert_eq!(m.signed_at, Some(SIGNED_AT));
    assert_ne!(m.sent_at, SIGNED_AT, "sent_at is the arrival");
    assert_eq!(a.unread(&sa), 1);
    assert!(lightbulb::holds::open(&a.store()).unwrap().is_empty());
}

/// An older build's A2A Message is refused, signed and well-formed or not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn envelope_with_a2a_is_refused_unsupported() {
    let (a, b, _ep, mut p) = a_and_peer_b().await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    for v in [
        signed(
            &b.identity(),
            envelope(&ulid(), &sb, &sa, "hi", Some(user("hi"))),
        ),
        envelope(&ulid(), &sb, &sa, "hi", Some(user("hi"))),
    ] {
        let id = v["id"].as_str().unwrap().to_string();
        p.send_raw(&v).await;
        assert_eq!(p.ack(&id).await, refused("unsupported"));
        assert!(a.store().message(&id).unwrap().is_none());
    }
    assert_eq!(a.unread(&sa), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_future_field_survives_signature_verification() {
    let (a, b, _ep, mut p) = a_and_peer_b().await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let mut v = envelope(&id, &sb, &sa, "hi", None);
    v["future"] = json!(1);
    let v = signed(&b.identity(), v);
    p.send_raw(&v).await;
    assert_eq!(p.ack(&id).await.0, "stored");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_signature_is_checked_before_the_kind() {
    let (a, b, _ep, mut p) = a_and_peer_b().await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let mut v = envelope(&id, &sb, &sa, "hi", None);
    v["kind"] = "input".into();
    let v = signed(&a.identity(), v);
    p.send_raw(&v).await;
    assert_eq!(p.ack(&id).await, refused("bad_request"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tampered_body_is_refused_bad_request() {
    let (a, b, _ep, mut p) = a_and_peer_b().await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let mut v = signed(&b.identity(), envelope(&id, &sb, &sa, "hi", None));
    v["body"] = "pay me".into();
    p.send_raw(&v).await;
    assert_eq!(p.ack(&id).await, refused("bad_request"));
    assert!(a.store().message(&id).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_signature_is_refused_bad_request() {
    let (a, b, _ep, mut p) = a_and_peer_b().await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let v = signed(&a.identity(), envelope(&id, &sb, &sa, "hi", None));
    p.send_raw(&v).await;
    assert_eq!(p.ack(&id).await, refused("bad_request"));
    assert!(a.store().message(&id).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_format_envelope_without_a2a_is_still_accepted() {
    let (a, b, _ep, mut p) = a_and_peer_b().await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let v = envelope(&id, &sb, &sa, "hello", None);
    assert!(v.get("sig").is_none());
    p.send_raw(&v).await;
    assert_eq!(p.ack(&id).await.0, "stored");
    let m = a.store().message(&id).unwrap().expect("stored");
    assert_eq!((m.sig, m.signed_at), (None, None));
    assert_eq!(a.unread(&sa), 1);
}
