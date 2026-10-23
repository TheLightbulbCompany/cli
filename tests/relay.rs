//! Relay fallback: two computers that cannot reach each other directly talk through a relay
//! (iroh's own relay server, run here over plain HTTP on the loopback); the signed relay
//! list; and a team's own relay, which replaces ours.

use iroh_relay::server::{RelayConfig, Server, ServerConfig};
use lightbulb_connect::config::Config;
use lightbulb_connect::relay;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

mod common;

use common::{out, peers, person, seed_pair, session, until, Env, EXE};

/// A relay server on the loopback, at `port` (0: any), and its URL.
async fn relay_server(port: u16) -> (Server, String) {
    let mut config = ServerConfig::default();
    config.relay = Some(RelayConfig::new((Ipv4Addr::LOCALHOST, port)));
    let server = Server::spawn(config).await.unwrap();
    let url = format!("http://{}", server.http_addr().unwrap());
    (server, url)
}

/// The debug knob that takes this daemon's direct (IP) transports away: relay only.
fn relay_only() -> (&'static str, String) {
    ("LIGHTBULB_CONNECT_DEBUG_RELAY_ONLY", "1".into())
}

/// The debug knob that trusts `kp`'s public key for `relays.json`.
fn trusts(kp: &minisign::KeyPair) -> (&'static str, String) {
    ("LIGHTBULB_CONNECT_DEBUG_RELAY_KEYS", kp.pk.to_base64())
}

fn sign(kp: &minisign::KeyPair, body: &[u8]) -> String {
    minisign::sign(Some(&kp.pk), &kp.sk, body, None, None)
        .unwrap()
        .to_string()
}

fn list_body(relays: &[&str]) -> Vec<u8> {
    serde_json::to_vec(&json!({"v": 1, "relays": relays, "expires_at": 4_000_000_000i64})).unwrap()
}

/// An issuer serving `relays.json` and its signature, counting the requests it gets.
fn issuer(body: Vec<u8>, sig: String) -> (String, Arc<AtomicUsize>) {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let url = format!("http://{}/connect", l.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            h.fetch_add(1, Ordering::SeqCst);
            let mut line = String::new();
            let mut r = BufReader::new(&s);
            r.read_line(&mut line).unwrap_or_default();
            while r.read_line(&mut String::new()).is_ok_and(|n| n > 2) {}
            let content = if line.contains("relays.json.minisig") {
                sig.clone().into_bytes()
            } else if line.contains("relays.json") {
                body.clone()
            } else {
                vec![]
            };
            let mut s = &s;
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                content.len()
            );
            let _ = s.write_all(&content);
        }
    });
    (url, hits)
}

fn online(e: &Env) -> bool {
    e.status()["peers"]["online"] == 1
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn messages_flow_via_relay_when_direct_is_blocked() {
    let (_server, url) = relay_server(0).await;
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    let cfg = format!("relay = \"{url}\"\n");
    b.daemon(false, &cfg, &[]);
    // A has no direct transport: every byte to or from it goes through the relay.
    a.daemon(false, &cfg, &[relay_only()]);
    until(30, "both online through the relay", || {
        online(&a) && online(&b)
    });
    until(30, "A's relay is up", || a.status()["relay"] == "ok");
    let s = a.status();
    assert_eq!(
        (&s["peers"]["direct"], &s["relay_error"]),
        (&json!(0), &Value::Null),
        "{s}"
    );
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    a.send(&sa, &sb, "via the relay", "r1", None);
    assert_eq!(b.take_one(&sb)["text"], "via the relay");
    // The relay saw only ciphertext: the log names no body either.
    let log = std::fs::read_to_string(a.state.path().join("daemon.log")).unwrap();
    assert!(!log.contains("via the relay"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_down_keeps_mail_queued() {
    // A fixed port, so the relay comes back at the same URL.
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let (server, url) = relay_server(port).await;
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    let cfg = format!("relay = \"{url}\"\n");
    b.daemon(false, &cfg, &[]);
    a.daemon(false, &cfg, &[relay_only()]);
    until(30, "both online through the relay", || {
        online(&a) && online(&b)
    });
    until(30, "A's relay is up", || a.status()["relay"] == "ok");

    server.shutdown().await.unwrap();
    until(30, "A sees the relay down", || {
        a.status()["relay"] == "unreachable"
    });
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    assert_eq!(a.send(&sa, &sb, "later", "r1", None)["state"], "queued");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(a.state(&sa, "r1"), "queued");
    assert_eq!(b.unread(&sb), 0);

    let (_server, again) = relay_server(port).await;
    assert_eq!(again, url);
    until(90, "stored once the relay is back", || {
        a.state(&sa, "r1") == "stored"
    });
    assert_eq!(b.take_one(&sb)["text"], "later");
}

#[test]
fn unsigned_or_partial_relay_list_is_refused() {
    let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let keys = vec![kp.pk.to_base64()];
    let body = list_body(&["https://r1.example", "https://r2.example"]);
    let sig = sign(&kp, &body);
    assert_eq!(
        relay::verify(&body, &sig, &keys).unwrap(),
        ["https://r1.example", "https://r2.example"]
    );
    // No signature, a signature by another key, a body cut short, a body changed: refused
    // whole, never a partial list.
    assert!(relay::verify(&body, "", &keys).is_err());
    let other = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    assert!(relay::verify(&body, &sign(&other, &body), &keys).is_err());
    assert!(relay::verify(&body[..body.len() - 20], &sig, &keys).is_err());
    let changed = list_body(&["https://r1.example", "https://evil.example"]);
    assert!(relay::verify(&changed, &sig, &keys).is_err());
    // Signed, but not a relay list.
    let junk = b"{\"v\":1}".to_vec();
    assert!(relay::verify(&junk, &sign(&kp, &junk), &keys).is_err());
    // The compiled-in keys are placeholders: a list our test key signed is refused by them.
    let compiled: Vec<String> = relay::KEYS.iter().map(|k| k.to_string()).collect();
    assert!(relay::verify(&body, &sig, &compiled).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn team_relay_from_meta_replaces_ours() {
    let (_server, team_relay) = relay_server(0).await;
    let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let ours = "http://127.0.0.1:1";
    let body = list_body(&[ours]);
    let sig = sign(&kp, &body);
    let (a_issuer, a_hits) = issuer(body.clone(), sig.clone());
    let (b_issuer, b_hits) = issuer(body.clone(), sig.clone());

    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    // B already holds our list (fresh), so it has no reason to fetch.
    std::fs::write(b.state.path().join("relays.json"), &body).unwrap();
    std::fs::write(b.state.path().join("relays.json.minisig"), &sig).unwrap();
    b.daemon(false, &format!("issuer = \"{b_issuer}\"\n"), &[trusts(&kp)]);
    // A has none: it fetches ours from its issuer and verifies it. A's daemon takes this test
    // process for an agent (a person's requests come through `person`).
    let me = std::process::id();
    let birth = lightbulb_connect::os::process(me).unwrap().birth_micros;
    let pin = ("LIGHTBULB_CONNECT_HOOK_CALLER", format!("{me}:{birth}"));
    a.daemon(
        false,
        &format!("issuer = \"{a_issuer}\"\n"),
        &[trusts(&kp), peers(&[&b]), pin],
    );
    until(10, "both online", || online(&a) && online(&b));
    let got = |e: &Env| e.ask(json!({"id": 1, "op": "relay.get"}));
    until(10, "A fetched ours", || got(&a)["source"] == "lightbulb");
    assert_eq!(got(&a)["urls"], json!([ours]));
    assert!(a_hits.load(Ordering::SeqCst) >= 2);
    assert_eq!(got(&b)["source"], "lightbulb");

    // The founder sets the team's relay: config here, `meta {relay}` for everyone.
    let r = person(&a, json!({"id": 1, "op": "relay.set", "url": team_relay}));
    assert_eq!(
        (r["ok"].as_bool(), r["team"].as_bool()),
        (Some(true), Some(true)),
        "{r}"
    );
    let config = Config::load(a.state.path()).unwrap();
    assert_eq!(config.relay.as_deref(), Some(team_relay.as_str()));
    assert_eq!(got(&a)["source"], "config");
    until(10, "B adopts the team's relay", || {
        got(&b)["source"] == "team"
    });
    assert_eq!(got(&b)["urls"], json!([team_relay]));
    until(30, "B reaches the team's relay", || {
        b.status()["relay"] == "ok"
    });
    // A teammate's ticket carries the relay in effect.
    let t = b.ask(json!({"id": 1, "op": "team.invite"}));
    let ticket = lightbulb_connect::ticket::decode(t["ticket"].as_str().unwrap()).unwrap();
    assert_eq!(ticket.relay_hint, team_relay);
    assert_eq!(b_hits.load(Ordering::SeqCst), 0, "B contacted our issuer");

    // With a team relay, even a stale cache contacts nothing of ours.
    let roster =
        lightbulb_connect::roster::replay(&lightbulb_connect::roster::entries(&b.store()).unwrap());
    let stale = tempfile::tempdir().unwrap();
    let config = Config {
        issuer: b_issuer.clone(),
        ..Config::default()
    };
    let l = relay::list(
        stale.path(),
        &config,
        &roster.relay,
        &[kp.pk.to_base64()],
        true,
    );
    assert_eq!(
        (l.source, l.urls),
        (relay::Source::Team, vec![team_relay.clone()])
    );
    assert_eq!(
        b_hits.load(Ordering::SeqCst),
        0,
        "a team relay contacts no issuer"
    );

    // `relay` alone prints the list and its source.
    let o = a.detached(&[EXE, "relay"], "");
    assert_eq!(out(&o), format!("config  {team_relay}\n"));
    // An agent may not move the team's traffic.
    let r: Value = a.ask(json!({"id": 1, "op": "relay.set", "url": "http://x"}));
    assert_eq!(r["reason"], "not_a_person", "{r}");
}
