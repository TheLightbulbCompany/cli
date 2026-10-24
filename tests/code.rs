//! The invite code: a join with four words over the code rendezvous, on a loopback relay.

mod common;

use common::{computer, dialog, members, person, until, Env};
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, RelayMode};
use iroh_relay::server::{RelayConfig, Server, ServerConfig};
use lightbulb::code;
use serde_json::{json, Value};
use std::net::Ipv4Addr;

fn relay_only() -> (&'static str, String) {
    ("LIGHTBULB_CONNECT_DEBUG_RELAY_ONLY", "1".into())
}

async fn relay_server() -> (Server, String) {
    let mut config = ServerConfig::default();
    config.relay = Some(RelayConfig::new((Ipv4Addr::LOCALHOST, 0)));
    let server = Server::spawn(config).await.unwrap();
    let url = format!("http://{}", server.http_addr().unwrap());
    (server, url)
}

/// A founder whose dialogs Allow, and its invite's code.
fn inviter(url: &str) -> (Env, String, Value) {
    let a = Env::new();
    a.daemon(
        false,
        &format!("{}relay = \"{url}\"\n", computer("mac")),
        &[dialog("Allow"), relay_only()],
    );
    let r = a.ask(json!({"id": 1, "op": "team.invite"}));
    assert_eq!(r["ok"], true, "{r}");
    let code = r["code"].as_str().expect("an invite carries a code").into();
    (a, code, r)
}

fn joiner(url: &str, name: &str) -> Env {
    let b = Env::new();
    b.daemon(
        false,
        &format!("{}relay = \"{url}\"\n", computer(name)),
        &[relay_only()],
    );
    b
}

fn join(e: &Env, code: &str) -> Value {
    person(
        e,
        json!({"id": 1, "op": "team.join", "code": code, "name": "bob"}),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_code_joins_once_and_a_second_try_or_a_wrong_code_fails() {
    let (_server, url) = relay_server().await;
    let (a, code, r) = inviter(&url);
    assert!(r["link"]
        .as_str()
        .unwrap()
        .starts_with("https://thelightbulb.company/j#"));
    assert!(!r["line"].as_str().unwrap().contains("NAME"));
    // Typed loosely: spaces and capitals.
    let b = joiner(&url, "mbp");
    let r = join(&b, &code.replace('-', " ").to_uppercase());
    assert_eq!(r["state"], "admitted", "{r}");
    until(10, "A lists B", || members(&a).len() == 2);
    // Used: a second computer gets nothing from the same code.
    let c = joiner(&url, "air");
    let r = join(&c, &code);
    assert_eq!(
        (r["state"].as_str(), r["reason"].as_str()),
        (Some("refused"), Some("no_code")),
        "{r}"
    );
    // Wrong: some other code, and a string that is no code at all.
    let other = (0..)
        .map(|_| code::generate())
        .find(|o| *o != code)
        .unwrap();
    for bad in [other.as_str(), "not a code"] {
        let r = join(&c, bad);
        assert_eq!(r["reason"], "no_code", "{r}");
        assert!(!r.to_string().contains(bad), "the answer repeats the code");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_bad_callers_close_the_listener() {
    let (_server, url) = relay_server().await;
    let (_a, code, _) = inviter(&url);
    let relay: iroh::RelayUrl = url.parse().unwrap();
    let ep = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Custom(
            [iroh::RelayConfig::new(relay.clone(), None)]
                .into_iter()
                .collect(),
        ))
        .bind()
        .await
        .unwrap();
    let to = || EndpointAddr::new(code::secret(&code).public()).with_relay_url(relay.clone());
    let mut served = 0;
    for i in 0..7 {
        let Ok(Ok(conn)) = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            ep.connect(to(), code::ALPN),
        )
        .await
        else {
            assert!(i >= 5, "the listener closed after {i} callers");
            continue;
        };
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        send.write_all(b"let me in").await.unwrap();
        send.finish().unwrap();
        let got = recv.read_to_end(8192).await.unwrap_or_default();
        assert!(got.is_empty(), "a bad caller got an answer");
        served += 1;
    }
    assert_eq!(served, 5, "exactly five bad callers are answered");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_code_for_an_open_link_still_asks_the_inviter() {
    let (_server, url) = relay_server().await;
    let a = Env::new();
    a.daemon(
        false,
        &format!("{}relay = \"{url}\"\n", computer("mac")),
        &[dialog("Decline"), relay_only()],
    );
    let r = person(&a, json!({"id": 1, "op": "team.invite", "mode": "open"}));
    let code = r["code"].as_str().expect("a code").to_string();
    let r = join(&joiner(&url, "mbp"), &code);
    assert_eq!(
        r["reason"], "declined",
        "an open link admits at once, a code asks: {r}"
    );
}
