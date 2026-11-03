//! The backend delivers the signed rules document over the route link (`rules.set`); the
//! person reads what is stored (`rules.get`).

mod common;

use common::{issuer, Env};
use data_encoding::BASE64URL_NOPAD;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_websockets::{Message, ServerBuilder};

type Ws = tokio_websockets::WebSocketStream<tokio::net::TcpStream>;

fn claims(workspace: &str, revision: &str, iat: i64) -> Value {
    json!({"workspace": workspace, "revision": revision, "iat": iat,
        "doc": {"revision": "x", "holders": {"session:*": ["@session-person"]},
                "rules": [{"id": "r1", "resource": "session:*", "skill": "message",
                           "requesters": ["org:*"], "effect": "allow"}],
                "default": "hold"}})
}

fn mint(workspace: &str, revision: &str, iat: i64) -> String {
    issuer::mint_typ("lightbulb-a2a-rules", &claims(workspace, revision, iat))
}

/// The same document signed by a key the daemon does not know.
fn forged(iat: i64) -> String {
    let b64 = |v: &Value| BASE64URL_NOPAD.encode(v.to_string().as_bytes());
    let signing = format!(
        "{}.{}",
        b64(&json!({"alg": "EdDSA", "kid": issuer::KID, "typ": "lightbulb-a2a-rules"})),
        b64(&claims("ws1", "evil", iat))
    );
    let sig = SigningKey::from_bytes(&[7u8; 32]).sign(signing.as_bytes());
    format!("{signing}.{}", BASE64URL_NOPAD.encode(&sig.to_bytes()))
}

async fn linked(e: &Env) -> (Ws, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "ws://{}/api/v1/gateway/network",
        listener.local_addr().unwrap()
    );
    e.sign_in("user_aden", Some(&url));
    e.daemon(false, "computer = \"Aden's Mac\"", &[]);
    let (tcp, _) = tokio::time::timeout(Duration::from_secs(30), listener.accept())
        .await
        .expect("the daemon dials within 30 s")
        .unwrap();
    let (_, mut ws) = ServerBuilder::new().accept(tcp).await.unwrap();
    let c = json!({"type": "challenge", "challenge": "n".repeat(43)});
    ws.send(Message::text(c.to_string())).await.unwrap();
    ws.next().await.unwrap().unwrap(); // hello
    let ready = json!({"type": "ready", "person": "ab12cd34"});
    ws.send(Message::text(ready.to_string())).await.unwrap();
    (ws, listener)
}

async fn set(ws: &mut Ws, id: u64, jws: &str) -> Value {
    let f = json!({"id": id, "op": "rules.set", "jws": jws});
    ws.send(Message::text(f.to_string())).await.unwrap();
    loop {
        let m = tokio::time::timeout(Duration::from_secs(15), ws.next())
            .await
            .expect("an answer")
            .unwrap()
            .unwrap();
        let v: Value = serde_json::from_str(m.as_text().unwrap()).unwrap();
        if v["id"] == id {
            return v;
        }
    }
}

fn get(e: &Env) -> Value {
    common::person(e, json!({"id": 1, "op": "rules.get"}))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rules_set_from_the_test_issuer_is_stored_and_read_back() {
    let e = Env::new();
    let (mut ws, _l) = linked(&e).await;
    let now = common::now_s();
    assert_eq!(get(&e)["revision"], "default");
    let r = set(&mut ws, 1, &mint("ws1", "r1", now)).await;
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["result"], json!({"stored": true, "revision": "r1"}));
    let g = get(&e);
    assert_eq!(
        (
            g["workspace"].clone(),
            g["revision"].clone(),
            g["iat"].clone()
        ),
        (json!("ws1"), json!("r1"), json!(now)),
        "{g}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rules_set_from_another_key_is_refused() {
    let e = Env::new();
    let (mut ws, _l) = linked(&e).await;
    let r = set(&mut ws, 1, &forged(common::now_s())).await;
    assert_eq!(
        (r["ok"].clone(), r["error"].clone()),
        (json!(false), json!("bad_request")),
        "{r}"
    );
    assert_eq!(get(&e)["revision"], "default");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn older_iat_is_ignored() {
    let e = Env::new();
    let (mut ws, _l) = linked(&e).await;
    let now = common::now_s();
    set(&mut ws, 1, &mint("ws1", "new", now)).await;
    let r = set(&mut ws, 2, &mint("ws1", "old", now - 100)).await;
    assert_eq!(
        r["result"],
        json!({"stored": false, "revision": "new"}),
        "{r}"
    );
    assert_eq!(get(&e)["revision"], "new");
    // Another workspace is refused once this computer is pinned.
    let r = set(&mut ws, 3, &mint("ws2", "x", now + 10)).await;
    assert_eq!(r["error"], "bad_request", "{r}");
}
