//! Workspace proofs: an embedded instance shows its own sign-in in its `hello`.

use lightbulb::identity::Identity;
use lightbulb::net::frames::{envelope_sig, Frame};
use lightbulb::proof;
use lightbulb::roster::{self, Entry, Kind};
use lightbulb::store::Store;
use lightbulb::workers;
use serde_json::{json, Value};

mod common;

use common::{endpoint, issuer, now_s, person, seed_team, session, ulid, until, Env, Peer};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_instance_presents_its_login_json_in_its_hello() {
    let home = tempfile::tempdir().unwrap();
    let w = Identity::load_or_create(home.path()).unwrap();
    let who = common::identity();
    let g = Entry::signed(
        &who,
        vec![],
        Kind::Admit,
        json!({"member": who.key, "name": "bob", "computer": "mac",
            "role": "founder", "ticket_id": "", "team_name": "acme"}),
        1,
    );
    let aw = common::admit(&who, &g, &w, "workspace", json!({}));
    let store = Store::open(&home.path().join("state.db")).unwrap();
    for e in [&g, &aw] {
        roster::accept(&store, e, 10).unwrap();
    }
    drop(store);
    let key = w.key.clone();
    let token = issuer::workspace_proof(&key, "org_1", now_s());
    let v = json!({"proof": token, "route_url": null, "name": "workspace"});
    std::fs::write(home.path().join("login.json"), v.to_string()).unwrap();
    let i = common::instance(home.path(), &[], "workspace", None, &[])
        .await
        .unwrap();
    let (mut peer, _ep) = common::dial_instance(&i, &who).await;
    let Frame::Hello { proof: shown, .. } =
        peer.recv_where(|f| matches!(f, Frame::Hello { .. })).await
    else {
        unreachable!()
    };
    let p = proof::verify(&shown.expect("a proof"), &key, now_s()).unwrap();
    assert_eq!(p.sub, "org_1");
    i.shutdown().await;
}

/// A signed envelope of `kind` (an assign with terms) from `from` to `to`, carrying A2A Message `a2a` when given.
fn envelope(
    who: &Identity,
    kind: &str,
    from: &str,
    to: &str,
    a2a: Option<Value>,
) -> (String, Value) {
    let id = ulid();
    let f = Frame::Envelope {
        id: id.clone(),
        request_id: format!("r-{id}"),
        kind: kind.into(),
        from: from.into(),
        to: to.into(),
        reply_to: None,
        thread: None,
        hops: 0,
        body: "hi".into(),
        files: vec![],
        sent_at: 0,
        meta: Default::default(),
        a2a,
        sig: None,
    };
    let mut v = serde_json::to_value(&f).unwrap();
    if kind == "assign" {
        v["meta"] = json!({"done_when": "done", "deadline_at": now_s() + 600});
    }
    v["sig"] = envelope_sig(who, &v).into();
    (id, v)
}

async fn ack(p: &mut Peer, id: &str) -> (String, Option<String>) {
    match p
        .recv_where(|f| matches!(f, Frame::Ack { id: i, .. } if i == id))
        .await
    {
        Frame::Ack { state, reason, .. } => (state, reason),
        _ => unreachable!(),
    }
}

/// A running (signed in as `user_a`, `config`), B played by the test, its `hello` showing a
/// workspace proof for `org_1`: what A sees of B's runtime signed in as its workspace.
async fn a_and_workspace_b(
    config: &str,
    env: &[(&str, String)],
) -> (Env, Env, iroh::Endpoint, Peer) {
    let (a, b) = (Env::new(), Env::new());
    seed_team(&a, &b);
    a.sign_in("user_a", None);
    a.daemon(false, config, env);
    let ep = endpoint(&b.identity()).await;
    let mut p = Peer::dial(&ep, &a).await;
    let proof = issuer::workspace_proof(&b.identity().key, "org_1", now_s());
    p.hello_with(vec![], Some(proof)).await;
    until(5, "B online", || a.status()["peers"]["online"] == 1);
    (a, b, ep, p)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_workspace_sender_without_a_note_is_judged_as_its_member() {
    let (a, b, _ep, mut p) = a_and_workspace_b("", &[]).await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    // A plain message: `members-message` allows it.
    let (id, v) = envelope(&b.identity(), "message", &sb, &sa, None);
    p.send_raw(&v).await;
    assert_eq!(ack(&mut p, &id).await.0, "stored");
    // An A2A ask without a note: held, judged as B's member, never as person `org_1`.
    let ask = json!({"role": "ROLE_USER", "parts": [{"text": "hi"}]});
    let (id, v) = envelope(&b.identity(), "message", &sb, &sa, Some(ask));
    p.send_raw(&v).await;
    assert_eq!(ack(&mut p, &id).await.0, "held");
    let h = lightbulb::holds::get(&a.store(), 1)
        .unwrap()
        .expect("a hold");
    assert_eq!(h.requester, Some(format!("member:{}", b.short())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_workspace_senders_assign_to_new_still_matches_its_member_row() {
    let work = tempfile::tempdir().unwrap();
    let config = format!(
        "network_workers = true\nworkers_offer = \"claude\"\nworkers_folder = \"{}\"\n",
        work.path().display()
    );
    let env = [(
        "LIGHTBULB_CONNECT_DEBUG_CLAUDE",
        "/usr/bin/false".to_string(),
    )];
    for (scope, want) in [
        ("always", ("stored", None)),
        ("refused", ("refused", Some("declined"))),
    ] {
        let (a, b) = (Env::new(), Env::new());
        seed_team(&a, &b);
        a.sign_in("user_a", None);
        workers::set_policy(
            &a.store(),
            &a.identity(),
            &b.short(),
            scope,
            "local",
            now_s(),
        )
        .unwrap();
        a.daemon(false, &config, &env);
        let ep = endpoint(&b.identity()).await;
        let mut p = Peer::dial(&ep, &a).await;
        let proof = issuer::workspace_proof(&b.identity().key, "org_1", now_s());
        p.hello_with(vec![], Some(proof)).await;
        until(5, "B online", || a.status()["peers"]["online"] == 1);
        let to = format!("{}/claude:new", a.short());
        let (id, v) = envelope(&b.identity(), "assign", &session(&b, "b1"), &to, None);
        p.send_raw(&v).await;
        let got = ack(&mut p, &id).await;
        assert_eq!((got.0.as_str(), got.1.as_deref()), want, "{scope}");
        assert!(
            lightbulb::holds::open(&a.store()).unwrap().is_empty(),
            "{scope}"
        );
    }
}

/// A hosted agent's note for `user_p` of `org`: two hops, the second `friday`'s.
fn relayed_note(org: &str, resource: &str) -> String {
    let (now, operands) = (now_s(), json!({"text": "hi"}));
    issuer::mint_typ(
        lightbulb::authority::TYP,
        &json!({
            "v": 1, "id": "auth_r", "iat": now, "exp": now + 600,
            "requester": {"person": "user_p", "org": org, "verified_by": "lightbulb"},
            "chain": [{"agent": null, "session": null, "person": "user_p", "at": now},
                      {"agent": "friday", "session": "run1", "person": "user_p", "at": now}],
            "operation": {"skill": "ask_session", "resource": resource, "operands": operands,
                          "operands_hash": lightbulb::authority::operands_hash(&operands)},
            "intent": "say hi", "policy": null,
        }),
    )
}

fn relayed_ask(jws: &str) -> Value {
    json!({"role": "ROLE_USER", "parts": [{"text": "hi"}],
           "metadata": {lightbulb::authority::URI: jws},
           "extensions": [lightbulb::authority::URI]})
}

fn open_holds(e: &Env) -> Vec<Value> {
    person(e, json!({"id": 1, "op": "holds"}))["holds"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relayed_hosted_ask_is_held_and_names_the_workspace() {
    let (a, b, _ep, mut p) = a_and_workspace_b("", &[]).await;
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let jws = relayed_note("org_1", &format!("session:{sa}"));
    let (id, v) = envelope(&b.identity(), "message", &sb, &sa, Some(relayed_ask(&jws)));
    p.send_raw(&v).await;
    assert_eq!(ack(&mut p, &id).await.0, "held");
    until(5, "the dialog's text kept", || {
        open_holds(&a)
            .first()
            .is_some_and(|h| h["shown"].is_string())
    });
    let h = &open_holds(&a)[0];
    assert_eq!(h["kind"], "authority");
    let shown = h["shown"].as_str().unwrap().to_string();
    assert!(shown.contains("via your workspace's agent"), "{shown}");
    let r = person(
        &a,
        json!({"id": 1, "op": "hold.answer", "hold": "H1", "verdict": "allow", "scope": "once"}),
    );
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(a.inbox(&sa).len(), 1);
    let c = rusqlite::Connection::open(a.state.path().join("state.db")).unwrap();
    let rec: String = c
        .query_row("SELECT record FROM approvals WHERE hold_id = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let rec: Value = serde_json::from_str(&rec).unwrap();
    assert_eq!(rec["relayed_by"], "org_1");
    assert_eq!(rec["text"], shown.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hosted_ask_from_another_workspace_is_refused_and_stores_nothing() {
    let (a, b) = (Env::new(), Env::new());
    seed_team(&a, &b);
    a.sign_in("user_a", None);
    a.daemon(false, "", &[]);
    let ep = endpoint(&b.identity()).await;
    let mut p = Peer::dial(&ep, &a).await;
    let proof = issuer::workspace_proof(&b.identity().key, "org_2", now_s());
    p.hello_with(vec![], Some(proof)).await;
    until(5, "B online", || a.status()["peers"]["online"] == 1);
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    let jws = relayed_note("org_1", &format!("session:{sa}"));
    let (id, v) = envelope(&b.identity(), "message", &sb, &sa, Some(relayed_ask(&jws)));
    p.send_raw(&v).await;
    let got = ack(&mut p, &id).await;
    assert_eq!(
        (got.0.as_str(), got.1.as_deref()),
        ("refused", Some("bad_request"))
    );
    assert!(open_holds(&a).is_empty());
    assert!(a.store().message(&id).unwrap().is_none());
    assert!(a.inbox(&sa).is_empty());
}
