//! A hosted assistant acting as a person (ChatGPT, Claude.ai, Grok… through Lightbulb's hosted
//! MCP) sends through the workspace member, from the app session `{app}-{person}` the bridge
//! declared for it, titled `"<App> via <First name>"`. No computer of the person's is asked:
//! the hub-era relay to a person's own computer is gone, and a computer drops what the member
//! still sends of it.
//!
//! The world: person P ("Aden") on Mac A ("mac", off unless a test starts it) and teammate Q's
//! Mac B ("mbp"), both signed into workspace W1, and W1's member. One roster, seeded in every
//! store before anything starts.

mod common;

use common::{admit, call, now_s, online, person_proof, played, Env};
use lightbulb::embed::Instance;
use lightbulb::identity::Identity;
use lightbulb::net::frames::{self, Frame};
use lightbulb::policy;
use lightbulb::roster::{self, Entry, Kind};
use lightbulb::store::Store;
use serde_json::{json, Value};
use std::time::Duration;

const NAME: &str = "The Lightbulb Company";
const P: &str = "p-aden";
const Q: &str = "p-quinn";
/// P's ChatGPT: the session the bridge declares, and its title.
const APP: &str = "chatgpt-p-aden";
const TITLE: &str = "ChatGPT via Aden";

struct World {
    a: Env,
    b: Env,
    w1: tempfile::TempDir,
}

/// Person `pid` (proof name `name`) on `computer`, signed into `ws`.
fn person(who: &Identity, pid: &str, name: &str, computer: &str, ws: &str) -> Value {
    json!({"person_id": pid, "computer": computer,
        "proof": person_proof(who, pid, name, Some(ws))})
}

fn world() -> World {
    let (a, b) = (Env::new(), Env::new());
    let w1 = tempfile::tempdir().unwrap();
    let iw1 = Identity::load_or_create(w1.path()).unwrap();
    let ia = a.identity();
    let mut gb = json!({"member": ia.key, "name": "aden", "kind": "computer",
        "role": "founder", "ticket_id": "", "team_name": "acme"});
    for (k, v) in person(&ia, P, "Aden", "mac", "W1").as_object().unwrap() {
        gb[k] = v.clone();
    }
    let g = Entry::signed(&ia, vec![], Kind::Admit, gb, 1);
    let ib = b.identity();
    let e1 = admit(&ia, &g, &ib, "quinn", person(&ib, Q, "Quinn", "mbp", "W1"));
    let ws = json!({"kind": "cloud", "person_id": "ws:W1",
        "proof": common::ws_proof(&iw1, "W1", NAME)});
    let e2 = admit(&ia, &e1, &iw1, NAME, ws);
    let mut stores: Vec<Store> = [&a, &b].iter().map(|e| e.store()).collect();
    stores.push(Store::open(&w1.path().join("state.db")).unwrap());
    for s in &stores {
        for e in [&g, &e1, &e2] {
            roster::accept(s, e, 10).unwrap();
        }
    }
    // Q lets the workspace's agents send without asking.
    policy::set_switch(&b.store(), true, "local", 1).unwrap();
    World { a, b, w1 }
}

impl World {
    /// W1's member dialing B (A stays off), once B is online, with P's ChatGPT declared.
    async fn instance(&self) -> Instance {
        self.b.daemon(false, "", &[]);
        let i = common::instance(self.w1.path(), &[&self.b], NAME, None, &[])
            .await
            .unwrap();
        online(&i, 1).await;
        let app = json!({"name": APP, "agent": "chatgpt", "title": TITLE});
        ok(&i, "cloud.sessions.set", json!({ "sessions": [app] })).await;
        i
    }

    fn q(&self) -> String {
        common::session(&self.b, "q1")
    }

    fn app(&self) -> String {
        let w = Identity::load_or_create(self.w1.path()).unwrap();
        format!("{}/{APP}", w.short)
    }
}

async fn ok(i: &Instance, op: &str, body: Value) -> Value {
    let r = call(i, op, body).await;
    assert_eq!(r["ok"], true, "{op}: {r}");
    r
}

fn send_body(to: &str, rid: &str) -> Value {
    json!({"app": "chatgpt", "person": P, "to": to, "text": "hello from chatgpt",
        "request_id": rid})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_app_send_goes_from_the_declared_app_session_with_the_mac_off_for_the_person() {
    let w = world();
    let i = w.instance().await;
    let q = w.q();
    let r = ok(&i, "app.send", send_body(&q, "r1")).await;
    assert_eq!(
        (r["request_id"].as_str(), r["state"].as_str()),
        (Some("r1"), Some("stored")),
        "{r}"
    );
    let id = r["id"].as_str().expect("the message id").to_string();
    let m = w.b.take_one(&q);
    assert_eq!(
        (m["id"].as_str(), m["from"].as_str(), m["text"].as_str()),
        (
            Some(id.as_str()),
            Some(w.app().as_str()),
            Some("hello from chatgpt")
        )
    );
    // The same request again is the first one's receipt: nothing more is sent.
    let again = ok(&i, "app.send", send_body(&q, "r1")).await;
    assert_eq!(again["id"], id.as_str(), "{again}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(w.b.inbox(&q).is_empty());
    // P's Mac never ran.
    assert!(!w.a.state.path().join("sock").exists());
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_recipient_reads_it_as_app_via_person() {
    let w = world();
    let i = w.instance().await;
    let q = w.q();
    ok(&i, "app.send", send_body(&q, "r1")).await;
    let m = w.b.take_one(&q);
    assert_eq!(m["label"], TITLE, "{m}");
    // Q answers; the app reads it from its own session, taken and read.
    w.b.send(&q, &w.app(), "hi chatgpt", "q1", m["id"].as_str());
    let inbox = json!({"app": "chatgpt", "person": P});
    let mut records = vec![];
    for _ in 0..250 {
        records = ok(&i, "app.inbox", inbox.clone()).await["records"]
            .as_array()
            .unwrap()
            .clone();
        if !records.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(
        (records[0]["text"].as_str(), records[0]["from"].as_str()),
        (Some("hi chatgpt"), Some(q.as_str()))
    );
    common::until(10, "Q's reply reads read", || w.b.state(&q, "q1") == "read");
    let again = ok(&i, "app.inbox", inbox).await;
    assert_eq!(again["records"], json!([]), "{again}");
    // The app's own send, by its request id.
    let ask = json!({"app": "chatgpt", "person": P, "request_id": "r1"});
    let r = ok(&i, "app.receipt", ask).await;
    assert!(
        ["delivered", "read"].contains(&r["state"].as_str().unwrap()),
        "{r}"
    );
    let ask = json!({"app": "chatgpt", "person": P, "request_id": "nope"});
    assert_eq!(call(&i, "app.receipt", ask).await["error"], "not_found");
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_undeclared_app_person_pair_is_refused() {
    let w = world();
    let i = w.instance().await;
    let q = w.q();
    for (app, who) in [("chatgpt", Q), ("claude", P)] {
        let mut send = send_body(&q, "r1");
        (send["app"], send["person"]) = (app.into(), who.into());
        let bodies = [
            ("app.send", send),
            ("app.inbox", json!({"app": app, "person": who})),
            (
                "app.receipt",
                json!({"app": app, "person": who, "request_id": "r1"}),
            ),
        ];
        for (op, body) in bodies {
            let r = call(&i, op, body).await;
            assert_eq!(
                (r["ok"].clone(), r["error"].clone(), r["reason"].clone()),
                (json!(false), json!("refused"), json!("not_declared")),
                "{op} {app}-{who}: {r}"
            );
        }
    }
    // Not a session name at all.
    let r = call(&i, "app.inbox", json!({"app": "chat gpt", "person": P})).await;
    assert_eq!(r["error"], "bad_request", "{r}");
    assert!(w.b.inbox(&q).is_empty());
    // The directory the bridge forwards answers an instance.
    let d = ok(&i, "directory", json!({"query": "quinn"})).await;
    assert!(d["sessions"].is_array(), "{d}");
    i.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_computer_ignores_app_envelopes_and_decisions_from_the_member() {
    let w = world();
    w.a.daemon(false, "", &[]);
    let iw1 = Identity::load_or_create(w.w1.path()).unwrap();
    let (mut p, _ep) = played(&iw1, &w.a).await;
    let id = common::ulid();
    p.send(&Frame::Envelope {
        id: id.clone(),
        request_id: id,
        kind: "app_send".into(),
        from: format!("{}/chatgpt-app", iw1.short),
        to: format!("{}/chatgpt-app", w.a.short()),
        reply_to: None,
        thread: None,
        hops: 0,
        body: json!({"app": "chatgpt", "to": w.q(), "text": "x", "request_id": "x1",
            "for_person": P})
        .to_string(),
        files: vec![],
        sent_at: now_s(),
    })
    .await;
    p.send(&Frame::Decision {
        hold_id: "H1".into(),
        holding_member: w.a.identity().key,
        verdict: "allow".into(),
        scope: "once".into(),
        decided_by: P.into(),
        nonce: common::ulid(),
        decided_at: now_s(),
        sig: String::new(),
    })
    .await;
    // Nothing comes back but presence, and nothing was sent.
    let back = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match frames::read(&mut p.recv).await {
                Ok(Frame::Presence { .. }) => continue,
                other => return other,
            }
        }
    })
    .await;
    assert!(back.is_err(), "{back:?}");
    common::until(10, "both dropped", || {
        let d = &w.a.status()["dropped"];
        d["app"].as_u64() >= Some(1) && d["decision"].as_u64() >= Some(1)
    });
    let sent = "SELECT COUNT(*) FROM messages";
    assert_eq!(common::count(&w.a.state.path().join("state.db"), sent), 0);
    let log = std::fs::read_to_string(w.a.state.path().join("daemon.log")).unwrap();
    assert!(log.contains("dropped an app envelope"), "{log}");
    assert!(log.contains("dropped a decision frame"), "{log}");
}
