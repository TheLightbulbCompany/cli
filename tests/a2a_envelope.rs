//! Envelopes carrying an A2A Message and the sending computer's signature: the receiver
//! verifies `sig` over the raw JSON it got, checks `body` against the text parts, and keeps
//! `a2a`, `sig` and the signed `sent_at`. A real daemon (A) and B played by the test (`Peer`).

use lightbulb::authority;
use lightbulb::identity::Identity;
use lightbulb::net::frames::{envelope_sig, Frame};
use serde_json::{json, Value};

mod common;

use common::{endpoint, issuer, now_s, seed_pair, session, ulid, until};
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
}

/// A running alone, B played by the test (connected, past its `hello`).
async fn a_and_peer_b() -> (Env, Env, iroh::Endpoint, Peer) {
    a_and_peer_b_as(None).await
}

/// `a_and_peer_b`, B's `hello` showing a sign-in proof for Lightbulb user `user`.
async fn a_and_peer_b_as(user: Option<&str>) -> (Env, Env, iroh::Endpoint, Peer) {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    a.daemon(false, "", &[]);
    let ep = endpoint(&b.identity()).await;
    let mut p = Peer::dial(&ep, &a).await;
    let proof = user.map(|u| issuer::proof(&b.identity().key, u, now_s()));
    p.hello_with(vec![], proof).await;
    until(5, "B online", || a.status()["peers"]["online"] == 1);
    (a, b, ep, p)
}

fn refused(reason: &str) -> (String, Option<String>) {
    ("refused".into(), Some(reason.into()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signed_envelope_with_a2a_is_stored_with_sig_and_signed_at() {
    let (a, b, _ep, mut p) = a_and_peer_b().await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let v = signed(
        &b.identity(),
        envelope(&id, &sb, &sa, "hi", Some(user("hi"))),
    );
    p.send_raw(&v).await;
    assert_eq!(p.ack(&id).await.0, "stored");
    let m = a.store().message(&id).unwrap().expect("stored");
    assert_eq!(m.a2a, Some(user("hi")));
    assert_eq!(m.sig.as_deref(), v["sig"].as_str());
    assert_eq!(m.signed_at, Some(SIGNED_AT));
    assert_ne!(m.sent_at, SIGNED_AT, "sent_at is the arrival");
    assert_eq!(m.authority_id, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_future_field_survives_signature_verification() {
    let (a, b, _ep, mut p) = a_and_peer_b().await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let mut v = envelope(&id, &sb, &sa, "hi", Some(user("hi")));
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
    let v = signed(
        &a.identity(),
        envelope(&id, &sb, &sa, "hi", Some(user("hi"))),
    );
    p.send_raw(&v).await;
    assert_eq!(p.ack(&id).await, refused("bad_request"));
    assert!(a.store().message(&id).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a2a_without_sig_is_refused_bad_request() {
    let (a, b, _ep, mut p) = a_and_peer_b().await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    p.send_raw(&envelope(&id, &sb, &sa, "hi", Some(user("hi"))))
        .await;
    assert_eq!(p.ack(&id).await, refused("bad_request"));
    assert!(a.store().message(&id).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn body_differing_from_parts_is_refused_bad_request() {
    let (a, b, _ep, mut p) = a_and_peer_b().await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let v = signed(
        &b.identity(),
        envelope(&id, &sb, &sa, "hi", Some(user("hi\n"))),
    );
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
    assert!(v.get("a2a").is_none() && v.get("sig").is_none());
    p.send_raw(&v).await;
    assert_eq!(p.ack(&id).await.0, "stored");
    let m = a.store().message(&id).unwrap().expect("stored");
    assert_eq!((m.a2a, m.sig, m.signed_at), (None, None, None));
    assert_eq!(a.unread(&sa), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_text_part_is_refused_unsupported() {
    let (a, b, _ep, mut p) = a_and_peer_b().await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let a2a = json!({"role": "ROLE_USER", "parts": [{"file": {"uri": "https://x/y.png"}}]});
    let v = signed(&b.identity(), envelope(&id, &sb, &sa, "", Some(a2a)));
    p.send_raw(&v).await;
    assert_eq!(p.ack(&id).await, refused("unsupported"));
    assert!(a.store().message(&id).unwrap().is_none());
}

/// An authority object from `person` for `resource` binding `{text}`, minted by the test issuer.
fn authority(person: &str, resource: &str, text: &str) -> String {
    authority_for(person, resource, text, 600)
}

/// `authority`, living `life` seconds from now.
fn authority_for(person: &str, resource: &str, text: &str, life: i64) -> String {
    let (now, operands) = (now_s(), json!({"text": text}));
    issuer::mint_typ(
        authority::TYP,
        &json!({
            "v": 1, "id": "auth_1", "iat": now, "exp": now + life,
            "requester": {"person": person, "org": "org_1", "verified_by": "lightbulb"},
            "chain": [{"agent": "agent_1", "session": null, "person": person, "at": now}],
            "operation": {"skill": "message", "resource": resource, "operands": operands,
                          "operands_hash": authority::operands_hash(&operands)},
            "intent": "say hi", "policy": null,
        }),
    )
}

/// `user(text)` carrying `jws` as its authority object.
fn asking(text: &str, jws: String) -> Value {
    let mut m = user(text);
    m["metadata"] = json!({ authority::URI: jws });
    m["extensions"] = json!([authority::URI]);
    m
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn envelope_with_valid_authority_is_stored_with_authority_id() {
    let (a, b, _ep, mut p) = a_and_peer_b_as(Some("user_b")).await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let a2a = asking("hi", authority("user_b", &format!("session:{sa}"), "hi"));
    let v = signed(&b.identity(), envelope(&id, &sb, &sa, "hi", Some(a2a)));
    p.send_raw(&v).await;
    assert_eq!(p.ack(&id).await.0, "stored");
    let m = a.store().message(&id).unwrap().expect("stored");
    assert_eq!(m.authority_id.as_deref(), Some("auth_1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authority_requester_not_the_sender_is_refused() {
    let (a, b, _ep, mut p) = a_and_peer_b_as(Some("user_b")).await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let a2a = asking("hi", authority("user_x", &format!("session:{sa}"), "hi"));
    let v = signed(&b.identity(), envelope(&id, &sb, &sa, "hi", Some(a2a)));
    p.send_raw(&v).await;
    assert_eq!(p.ack(&id).await, refused("bad_request"));
    assert!(a.store().message(&id).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resend_after_the_authority_expired_answers_its_stored_state() {
    let (a, b, _ep, mut p) = a_and_peer_b_as(Some("user_b")).await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let a2a = asking(
        "hi",
        authority_for("user_b", &format!("session:{sa}"), "hi", 2),
    );
    let v = signed(&b.identity(), envelope(&id, &sb, &sa, "hi", Some(a2a)));
    p.send_raw(&v).await;
    assert_eq!(p.ack(&id).await.0, "stored");
    // No clock control in the harness: wait past the object's `exp`.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    p.send_raw(&v).await;
    assert_eq!(p.ack(&id).await.0, "stored");
}
