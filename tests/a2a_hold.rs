//! Every inbound envelope is judged by the workspace rules (`rules::decide`): allow stores it as
//! before, deny refuses `policy`, hold keeps it in the mailbox with receipt `held` and an open
//! Authority hold. A real daemon (A) and its teammate B played by the test (`Peer`).

use lightbulb::authority;
use lightbulb::identity::Identity;
use lightbulb::net::frames::{envelope_sig, Frame};
use serde_json::{json, Value};

mod common;

use common::{count, endpoint, issuer, now_s, person, seed_team, session, ulid, until, Env, Peer};

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
        sent_at: 0,
        meta: Default::default(),
        a2a,
        sig: None,
    };
    serde_json::to_value(&f).unwrap()
}

fn signed(who: &Identity, mut v: Value) -> Value {
    v["sig"] = envelope_sig(who, &v).into();
    v
}

/// An A2A ask of `text`, carrying authority object `jws` when given.
fn ask(text: &str, jws: Option<String>) -> Value {
    let mut m = json!({"role": "ROLE_USER", "parts": [{"text": text}]});
    if let Some(jws) = jws {
        m["metadata"] = json!({ authority::URI: jws });
        m["extensions"] = json!([authority::URI]);
    }
    m
}

/// An `ask_session` object from `person` for `resource` binding `{text}`.
fn authority(person: &str, resource: &str, text: &str) -> String {
    authority_in("org_1", person, resource, text)
}

/// `authority`, its requester in workspace `org`.
fn authority_in(org: &str, person: &str, resource: &str, text: &str) -> String {
    let (now, operands) = (now_s(), json!({"text": text}));
    issuer::mint_typ(
        authority::TYP,
        &json!({
            "v": 1, "id": "auth_1", "iat": now, "exp": now + 600,
            "requester": {"person": person, "org": org, "verified_by": "lightbulb"},
            "chain": [{"agent": "agent_1", "session": null, "person": person, "at": now}],
            "operation": {"skill": "ask_session", "resource": resource, "operands": operands,
                          "operands_hash": authority::operands_hash(&operands)},
            "intent": "say hi", "policy": null,
        }),
    )
}

/// Workspace `org_1`'s rules document with `rules`, holders `session:*` → `@session-person`.
fn document(rules: Value) -> String {
    issuer::mint_typ(
        "lightbulb-a2a-rules",
        &json!({"workspace": "org_1", "revision": "r1", "iat": now_s(),
            "doc": {"revision": "r1", "holders": {"session:*": ["@session-person"]},
                    "rules": rules, "default": "hold"}}),
    )
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

/// A running (signed in as `a_user`, holding `doc` when given), B played by the test, its
/// `hello` showing a proof for `b_user` when given.
async fn a_and_b(
    a_user: Option<&str>,
    b_user: Option<&str>,
    doc: Option<String>,
) -> (Env, Env, iroh::Endpoint, Peer) {
    let (a, b) = (Env::new(), Env::new());
    seed_team(&a, &b);
    if let Some(u) = a_user {
        a.sign_in(u, None);
    }
    if let Some(d) = doc {
        assert!(lightbulb::rules::store_doc(&a.store(), &d, now_s()).unwrap());
    }
    a.daemon(false, "", &[]);
    let ep = endpoint(&b.identity()).await;
    let mut p = Peer::dial(&ep, &a).await;
    let proof = b_user.map(|u| issuer::proof(&b.identity().key, u, now_s()));
    p.hello_with(vec![], proof).await;
    until(5, "B online", || a.status()["peers"]["online"] == 1);
    (a, b, ep, p)
}

fn held(e: &Env) -> Vec<Value> {
    person(e, json!({"id": 1, "op": "holds"}))["holds"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn mailbox(e: &Env) -> i64 {
    count(
        &e.state.path().join("state.db"),
        "SELECT COUNT(*) FROM mailbox",
    )
}

fn refused(reason: &str) -> (String, Option<String>) {
    ("refused".into(), Some(reason.into()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plain_member_message_with_no_document_is_allowed() {
    let (a, b, _ep, mut p) = a_and_b(None, None, None).await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    p.send_raw(&envelope(&id, &sb, &sa, "hello", None)).await;
    assert_eq!(p.ack(&id).await.0, "stored");
    assert_eq!(a.unread(&sa), 1);
    assert!(held(&a).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a2a_ask_with_no_document_is_held_for_the_session_person() {
    let (a, b, _ep, mut p) = a_and_b(Some("user_a"), Some("user_b"), None).await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let jws = authority("user_b", &format!("session:{sa}"), "hi");
    let v = signed(
        &b.identity(),
        envelope(&id, &sb, &sa, "hi", Some(ask("hi", Some(jws)))),
    );
    p.send_raw(&v).await;
    assert_eq!(p.ack(&id).await.0, "held");
    let open = held(&a);
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0]["kind"], "authority");
    assert_eq!(open[0]["target_session"], sa.as_str());
    let m = a.store().message(&id).unwrap().expect("stored");
    assert_eq!(m.authority_id.as_deref(), Some("auth_1"));
    // In the mailbox, but A's session does not get it until the person answers.
    assert_eq!(mailbox(&a), 1);
    assert_eq!(a.unread(&sa), 0);
    assert!(a.inbox(&sa).is_empty());
    assert_eq!(a.state(&sb, &format!("r-{id}")), "held");
    // A resend is told where it is, with no second hold.
    p.send_raw(&v).await;
    assert_eq!(p.ack(&id).await.0, "held");
    assert_eq!(held(&a).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deny_rule_refuses_policy() {
    let deny = json!([{"id": "no-b", "resource": "session:*", "skill": "message",
                       "requesters": ["person:user_b"], "effect": "deny"},
                      {"id": "members-message", "resource": "session:*", "skill": "message",
                       "requesters": ["org:*"], "effect": "allow"}]);
    let (a, b, _ep, mut p) = a_and_b(Some("user_a"), Some("user_b"), Some(document(deny))).await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    p.send_raw(&envelope(&id, &sb, &sa, "hello", None)).await;
    assert_eq!(p.ack(&id).await, refused("policy"));
    assert!(a.store().message(&id).unwrap().is_none());
    assert!(held(&a).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hold_with_no_holder_is_refused_policy() {
    // A is not signed in: `@session-person` names no one.
    let (a, b, _ep, mut p) = a_and_b(None, Some("user_b"), None).await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let jws = authority("user_b", &format!("session:{sa}"), "hi");
    let v = signed(
        &b.identity(),
        envelope(&id, &sb, &sa, "hi", Some(ask("hi", Some(jws)))),
    );
    p.send_raw(&v).await;
    assert_eq!(p.ack(&id).await, refused("policy"));
    assert!(a.store().message(&id).unwrap().is_none());
    assert!(held(&a).is_empty());
    assert_eq!(mailbox(&a), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsigned_sender_a2a_ask_is_held_as_member() {
    // B shows no proof, so it carries no object (one would not bind): an A2A ask as member.
    let (a, b, _ep, mut p) = a_and_b(Some("user_a"), None, None).await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let v = signed(
        &b.identity(),
        envelope(&id, &sb, &sa, "hi", Some(ask("hi", None))),
    );
    p.send_raw(&v).await;
    assert_eq!(p.ack(&id).await.0, "held");
    let open = held(&a);
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0]["kind"], "authority");
    let h = lightbulb::holds::open(&a.store()).unwrap().remove(0);
    assert_eq!(h.from_member, b.short());
    assert_eq!(h.message.authority_id, None);
    assert_eq!(a.unread(&sa), 0);
}

/// A bound object from `user_b` sent to A, whose document is `doc`: the ack and whether A wrote
/// anything (message, hold or mailbox row).
async fn object_against(doc: String, org: &str) -> ((String, Option<String>), bool) {
    let (a, b, _ep, mut p) = a_and_b(Some("user_a"), Some("user_b"), Some(doc)).await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let jws = authority_in(org, "user_b", &format!("session:{sa}"), "hi");
    let v = signed(
        &b.identity(),
        envelope(&id, &sb, &sa, "hi", Some(ask("hi", Some(jws)))),
    );
    p.send_raw(&v).await;
    let ack = p.ack(&id).await;
    let wrote =
        a.store().message(&id).unwrap().is_some() || !held(&a).is_empty() || mailbox(&a) > 0;
    (ack, wrote)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deny_rule_refuses_an_authority_object_too() {
    let deny = json!([{"id": "no-b", "resource": "session:*", "skill": "ask_session",
                       "requesters": ["person:user_b"], "effect": "deny"}]);
    let (ack, wrote) = object_against(document(deny), "org_1").await;
    assert_eq!(ack, refused("policy"));
    assert!(!wrote);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn object_from_another_workspace_is_refused_outside_org() {
    let allow = json!([{"id": "all", "resource": "session:*", "skill": "ask_session",
                        "requesters": ["*"], "effect": "allow"}]);
    let (ack, wrote) = object_against(document(allow), "org_2").await;
    assert_eq!(ack, refused("policy"));
    assert!(!wrote);
}
