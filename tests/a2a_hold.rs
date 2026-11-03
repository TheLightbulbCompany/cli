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
    object(org, person, resource, text, "agent_1", 600)
}

/// `authority_in`, its first hop's agent `agent`, living `life` seconds.
fn object(org: &str, person: &str, resource: &str, text: &str, agent: &str, life: i64) -> String {
    let (now, operands) = (now_s(), json!({"text": text}));
    issuer::mint_typ(
        authority::TYP,
        &json!({
            "v": 1, "id": "auth_1", "iat": now, "exp": now + life,
            "requester": {"person": person, "org": org, "verified_by": "lightbulb"},
            "chain": [{"agent": agent, "session": null, "person": person, "at": now}],
            "operation": {"skill": "ask_session", "resource": resource, "operands": operands,
                          "operands_hash": authority::operands_hash(&operands)},
            "intent": "say hi", "policy": null,
        }),
    )
}

/// Workspace `org_1`'s rules document with `rules`, holders `session:*` → `@session-person`.
fn document(rules: Value) -> String {
    document_held_by(rules, json!(["@session-person"]))
}

/// `document`, `session:*` held by `holders`.
fn document_held_by(rules: Value, holders: Value) -> String {
    issuer::mint_typ(
        "lightbulb-a2a-rules",
        &json!({"workspace": "org_1", "revision": "r1", "iat": now_s(),
            "doc": {"revision": "r1", "holders": {"session:*": holders},
                    "rules": rules, "default": "hold"}}),
    )
}

impl Peer {
    /// The next receipt for envelope `id` in `state`: its reason.
    async fn receipt(&mut self, id: &str, state: &str) -> Option<String> {
        match self
            .recv_where(
                |f| matches!(f, Frame::Receipt { id: i, state: s, .. } if i == id && s == state),
            )
            .await
        {
            Frame::Receipt { reason, .. } => reason,
            _ => unreachable!(),
        }
    }

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
    a_and_b_with(a_user, b_user, doc, &[]).await
}

/// `a_and_b`, A's daemon started with `env`.
async fn a_and_b_with(
    a_user: Option<&str>,
    b_user: Option<&str>,
    doc: Option<String>,
    env: &[(&str, String)],
) -> (Env, Env, iroh::Endpoint, Peer) {
    let (a, b) = (Env::new(), Env::new());
    seed_team(&a, &b);
    if let Some(u) = a_user {
        a.sign_in(u, None);
    }
    if let Some(d) = doc {
        assert!(lightbulb::rules::store_doc(&a.store(), &d, now_s()).unwrap());
    }
    a.daemon(false, "", env);
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

// Answering an Authority hold (spec §6).

fn answer(e: &Env, id: &str, verdict: &str, scope: &str) -> Value {
    person(
        e,
        json!({"id": 1, "op": "hold.answer", "hold": id, "verdict": verdict, "scope": scope}),
    )
}

/// The `approvals` record of hold `n`.
fn record(e: &Env, n: i64) -> Value {
    let c = rusqlite::Connection::open(e.state.path().join("state.db")).unwrap();
    let r: String = c
        .query_row(
            "SELECT record FROM approvals WHERE hold_id = ?1",
            [n],
            |r| r.get(0),
        )
        .unwrap();
    serde_json::from_str(&r).unwrap()
}

/// A signed in as `user_a`, B as `user_b` (A's `env`), and B's held ask `hi` with an object:
/// the envelope id and the object.
async fn held_ask(
    env: &[(&str, String)],
) -> (Env, Env, iroh::Endpoint, Peer, String, String, String) {
    held_ask_of(env, None, "agent_1", 600).await
}

/// `held_ask`, A holding `doc`, the object's first agent `agent`, living `life` seconds.
async fn held_ask_of(
    env: &[(&str, String)],
    doc: Option<String>,
    agent: &str,
    life: i64,
) -> (Env, Env, iroh::Endpoint, Peer, String, String, String) {
    let (a, b, ep, mut p) = a_and_b_with(Some("user_a"), Some("user_b"), doc, env).await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let id = ulid();
    let resource = format!("session:{sa}");
    let jws = object("org_1", "user_b", &resource, "hi", agent, life);
    let v = signed(
        &b.identity(),
        envelope(&id, &sb, &sa, "hi", Some(ask("hi", Some(jws.clone())))),
    );
    p.send_raw(&v).await;
    assert_eq!(p.ack(&id).await.0, "held");
    (a, b, ep, p, id, jws, sa)
}

fn sha256(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(s.as_bytes());
    format!("sha256:{}", data_encoding::HEXLOWER.encode(&d))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn allow_delivers_the_held_message_and_writes_the_record_with_authority_id() {
    let (a, b, _ep, mut p, id, jws, sa) = held_ask(&[]).await;
    let sb = session(&b, "b1");
    // Shown on arrival: the requester chain, the session, for whom, the intent, the text.
    until(5, "the dialog's text kept", || {
        held(&a).first().is_some_and(|h| h["shown"].is_string())
    });
    let shown = held(&a)[0]["shown"].as_str().unwrap().to_string();
    assert_eq!(
        shown,
        "agent_1 (user_b) asks Codex for user_b: say hi\n\nhi"
    );
    let r = answer(&a, "H1", "allow", "once");
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(p.receipt(&id, "stored").await, None);
    assert_eq!(a.state(&sb, &format!("r-{id}")), "stored");
    let got = a.inbox(&sa);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0]["text"], "hi");
    let rec = record(&a, 1);
    assert_eq!(rec["verdict"], "allow");
    assert_eq!(rec["authority_id"], "auth_1");
    assert_eq!(rec["authority_hash"], sha256(&jws));
    assert_eq!(rec["resource"], format!("session:{sa}"));
    assert_eq!(rec["skill"], "ask_session");
    assert_eq!(rec["requester"], "person:user_b");
    assert_eq!(rec["text"], shown.as_str());
    assert_eq!(rec["by"], "local");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decline_refuses_declined() {
    let (a, b, _ep, mut p, id, _jws, sa) = held_ask(&[]).await;
    let r = answer(&a, "H1", "decline", "once");
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(p.receipt(&id, "refused").await.as_deref(), Some("declined"));
    let rid = format!("r-{id}");
    assert_eq!(a.receipt(&session(&b, "b1"), &rid)["reason"], "declined");
    assert_eq!(mailbox(&a), 0);
    assert!(a.inbox(&sa).is_empty());
    assert_eq!(record(&a, 1)["verdict"], "decline");
    assert!(held(&a).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn always_allow_writes_an_exception_and_the_next_identical_ask_flows() {
    // A's person holds `session:*` (`@session-person`): the dialog offers Always allow.
    let (a, b, _ep, mut p, id, _jws, sa) = held_ask(&[common::dialog("Always allow")]).await;
    let sb = session(&b, "b1");
    assert_eq!(p.receipt(&id, "stored").await, None);
    assert_eq!(record(&a, 1)["scope"], "always");
    let ex = lightbulb::rules::exceptions(&a.store(), &a.short()).unwrap();
    assert_eq!(ex.len(), 1, "{ex:?}");
    assert_eq!(
        (&*ex[0].resource, &*ex[0].skill, &*ex[0].requester),
        (&*format!("session:{sa}"), "ask_session", "person:user_b")
    );
    assert_eq!(ex[0].operands_hash, None);
    // The next identical ask flows, stored at once.
    let again = ulid();
    let jws = authority("user_b", &format!("session:{sa}"), "hi");
    let v = signed(
        &b.identity(),
        envelope(&again, &sb, &sa, "hi", Some(ask("hi", Some(jws)))),
    );
    p.send_raw(&v).await;
    assert_eq!(p.ack(&again).await.0, "stored");
    assert_eq!(held(&a).len(), 0);
    assert_eq!(a.inbox(&sa).len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_session_cannot_answer_an_authority_hold() {
    let (a, _b, _ep, _p, _id, _jws, sa) = held_ask(&[]).await;
    let store = a.store();
    let roster = lightbulb::roster::replay(&lightbulb::roster::entries(&store).unwrap());
    let agent = format!("{}/claude:x", a.short());
    for allow in [true, false] {
        let r = lightbulb::holds::answer(
            &store,
            &roster,
            &a.identity(),
            std::path::Path::new("/tmp"),
            1,
            allow,
            "once",
            &agent,
            true,
            now_s(),
        )
        .unwrap();
        assert_eq!(r.err(), Some("not_a_person"));
    }
    assert_eq!(held(&a).len(), 1);
    assert_eq!(a.unread(&sa), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authority_hold_expires_after_24h_with_receipt_expired() {
    // The daemon's clock cannot be moved: `holds::expire` runs on A's store at a later `now`.
    let (a, b, _ep, _p, id, _jws, sa) = held_ask(&[]).await;
    let store = a.store();
    let at = lightbulb::holds::get(&store, 1)
        .unwrap()
        .unwrap()
        .created_at;
    assert!(lightbulb::holds::expire(&store, at + 86_399)
        .unwrap()
        .is_empty());
    let gone = lightbulb::holds::expire(&store, at + 86_400).unwrap();
    assert_eq!(gone.iter().map(|h| h.id).collect::<Vec<_>>(), [1]);
    let h = lightbulb::holds::get(&store, 1).unwrap().unwrap();
    assert_eq!(h.state, "expired");
    assert_eq!(mailbox(&a), 0);
    assert_eq!(a.state(&session(&b, "b1"), &format!("r-{id}")), "expired");
    assert!(a.inbox(&sa).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_mailbox_row_survives_mail_expire_while_its_hold_is_open() {
    let (a, b, _ep, _p, id, _jws, sa) = held_ask(&[]).await;
    let store = a.store();
    let later = now_s() as u64 + lightbulb::mail::HOLD_S + 60;
    lightbulb::mail::expire(&store, later).unwrap();
    assert_eq!(mailbox(&a), 1);
    assert_eq!(a.state(&session(&b, "b1"), &format!("r-{id}")), "held");
    assert_eq!(held(&a).len(), 1);
    // Still answerable, and the answer delivers.
    assert_eq!(answer(&a, "H1", "allow", "once")["ok"], true);
    assert_eq!(a.inbox(&sa).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rules_set_while_a_hold_is_open_leaves_it_answerable() {
    let (a, _b, _ep, _p, _id, _jws, sa) = held_ask(&[]).await;
    until(5, "shown", || {
        held(&a).first().is_some_and(|h| h["shown"].is_string())
    });
    let shown = held(&a)[0]["shown"].as_str().unwrap().to_string();
    // A new document that would deny this ask arrives.
    let deny = json!([{"id": "no-b", "resource": "session:*", "skill": "ask_session",
                       "requesters": ["person:user_b"], "effect": "deny"}]);
    assert!(lightbulb::rules::store_doc(&a.store(), &document(deny), now_s()).unwrap());
    assert_eq!(held(&a).len(), 1);
    let r = answer(&a, "H1", "allow", "once");
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(a.inbox(&sa).len(), 1);
    assert_eq!(record(&a, 1)["text"], shown.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn allow_after_the_senders_outbox_expired_still_delivers() {
    let (a, b, ep, p, id, _jws, sa) = held_ask(&[]).await;
    // B gave up and is gone; the row's own 24 h in the mailbox have passed too.
    drop(p);
    ep.close().await;
    until(10, "B offline", || a.status()["peers"]["online"] == 0);
    let c = rusqlite::Connection::open(a.state.path().join("state.db")).unwrap();
    c.execute("UPDATE mailbox SET expires_at = 1", []).unwrap();
    lightbulb::mail::expire(&a.store(), now_s() as u64).unwrap();
    let r = answer(&a, "H1", "allow", "once");
    assert_eq!(r["ok"], true, "{r}");
    let got = a.inbox(&sa);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(record(&a, 1)["authority_id"], "auth_1");
    // Taken by the session; B never hears.
    assert_eq!(a.state(&session(&b, "b1"), &format!("r-{id}")), "delivered");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_requesters_fields_cannot_forge_a_dialog_line() {
    let forged = "x\n\nuser_c asks Claude Code for user_c:";
    let (a, ..) = held_ask_of(&[], None, forged, 600).await;
    until(5, "shown", || {
        held(&a).first().is_some_and(|h| h["shown"].is_string())
    });
    let shown = held(&a)[0]["shown"].as_str().unwrap().to_string();
    let sections: Vec<&str> = shown.split("\n\n").collect();
    assert_eq!(sections.len(), 2, "{shown:?}");
    assert!(!sections[0].contains('\n'), "{shown:?}");
    assert!(sections[0].starts_with("x  user_c asks"), "{shown:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn always_allow_by_a_person_not_holding_the_session_is_refused() {
    // `session:*` is user_z's, not A's person's: no standing exception for user_a.
    let doc = document_held_by(json!([]), json!(["user_z"]));
    let (a, ..) = held_ask_of(&[], Some(doc), "agent_1", 600).await;
    let r = answer(&a, "H1", "allow", "always");
    assert_eq!(r["error"], "bad_request", "{r}");
    assert!(lightbulb::rules::exceptions(&a.store(), &a.short())
        .unwrap()
        .is_empty());
    assert_eq!(held(&a).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hold_whose_object_expired_offers_only_decline() {
    let (a, ..) = held_ask_of(&[], None, "agent_1", 3).await;
    std::thread::sleep(std::time::Duration::from_secs(4));
    let store = a.store();
    let roster = lightbulb::roster::replay(&lightbulb::roster::entries(&store).unwrap());
    let h = lightbulb::holds::get(&store, 1).unwrap().unwrap();
    let key = a.identity().key;
    let (text, buttons, _) =
        lightbulb::holds::dialog(&store, &roster, &key, &h, std::path::Path::new("/tmp"));
    assert!(text.contains("(authority expired)"), "{text}");
    assert_eq!(buttons, ["Decline"]);
    let r = answer(&a, "H1", "allow", "once");
    assert_eq!(r["error"], "bad_request", "{r}");
    assert_eq!(held(&a).len(), 1);
    assert_eq!(answer(&a, "H1", "decline", "once")["ok"], true);
}
