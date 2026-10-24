//! Relay fallback: two computers that cannot reach each other directly talk through a relay
//! (iroh's own relay server, run here over plain HTTP on the loopback); the signed relay
//! list; and a team's own relay, which replaces ours.

use iroh_relay::server::{RelayConfig, Server, ServerConfig};
use lightbulb::config::Config;
use lightbulb::relay;
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

/// `url` as the engine writes it (`RelayUrl`'s form: a trailing `/`).
fn canon(url: &str) -> String {
    relay::parse(url).unwrap().to_string()
}

/// What the engine makes of the list in `dir` once a fetch from `issuer` was due: our list
/// (no relay of its own set).
fn ours_after_fetch(dir: &std::path::Path, issuer: &str, keys: &[String]) -> relay::List {
    let mut s = relay::State::load(dir, keys);
    assert!(s.fetch_if_due(dir, issuer, (&None, &None), keys));
    relay::list(&None, &None, &s)
}

fn list_body(relays: &[&str]) -> Vec<u8> {
    list_at(relays, 4_000_000_000)
}

fn list_at(relays: &[&str], expires_at: i64) -> Vec<u8> {
    serde_json::to_vec(&json!({"v": 1, "relays": relays, "expires_at": expires_at})).unwrap()
}

/// `dir`'s cached `relays.json` made two days old: the next fetching `list` asks the issuer.
fn age(dir: &std::path::Path) {
    let old = std::time::SystemTime::now() - Duration::from_secs(2 * 86_400);
    let f = std::fs::File::options()
        .write(true)
        .open(dir.join("relays.json"))
        .unwrap();
    f.set_modified(old).unwrap();
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
    assert_eq!(a.status()["relay_error"], Value::Null);
    // B has direct transports, yet its only path to A is the relay: not counted as direct
    // (a direct link counts 1: `net_two`).
    let s = b.status();
    assert_eq!(
        (&s["peers"]["online"], &s["peers"]["direct"]),
        (&json!(1), &json!(0)),
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

/// The relay going down takes nothing from two computers that also reach each other
/// directly (one network): they stay online, and a message sent after the outage is stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_down_leaves_the_direct_path_working() {
    let (server, url) = relay_server(0).await;
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    let cfg = format!("relay = \"{url}\"\n");
    b.daemon(false, &cfg, &[]);
    a.daemon(false, &cfg, &[peers(&[&b])]);
    until(30, "A's relay is up", || a.status()["relay"] == "ok");
    until(30, "a direct link", || a.status()["peers"]["direct"] == 1);

    server.shutdown().await.unwrap();
    until(30, "A sees the relay down", || {
        a.status()["relay"] == "unreachable"
    });
    assert!(online(&a) && online(&b));
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    a.send(&sa, &sb, "same network", "r1", None);
    assert_eq!(b.take_one(&sb)["text"], "same network");
}

#[test]
fn unsigned_or_partial_relay_list_is_refused() {
    let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let keys = vec![kp.pk.to_base64()];
    let body = list_body(&["https://r1.example", "https://r2.example"]);
    let sig = sign(&kp, &body);
    let urls: Vec<String> = relay::verify(&body, &sig, &keys)
        .unwrap()
        .0
        .iter()
        .map(|u| u.to_string())
        .collect();
    assert_eq!(
        urls,
        [canon("https://r1.example"), canon("https://r2.example")]
    );
    // No signature, a signature by another key, a body cut short, a body changed: refused
    // whole, never a partial list.
    assert!(relay::verify(&body, "", &keys).is_err());
    let other = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    assert!(relay::verify(&body, &sign(&other, &body), &keys).is_err());
    assert!(relay::verify(&body[..body.len() - 20], &sig, &keys).is_err());
    let changed = list_body(&["https://r1.example", "https://evil.example"]);
    assert!(relay::verify(&changed, &sig, &keys).is_err());
    // More than 8 relays, or one that is not an https relay URL: refused whole.
    let nine: Vec<String> = (0..9).map(|i| format!("https://r{i}.example")).collect();
    let nine: Vec<&str> = nine.iter().map(String::as_str).collect();
    let long = list_body(&nine);
    assert!(relay::verify(&long, &sign(&kp, &long), &keys).is_err());
    let eight = list_body(&nine[..8]);
    assert_eq!(
        relay::verify(&eight, &sign(&kp, &eight), &keys)
            .unwrap()
            .0
            .len(),
        8
    );
    for bad in [
        "ftp://r.example",
        "https://u@r.example",
        "https://r.example/?a=1",
    ] {
        let b = list_body(&["https://r1.example", bad]);
        assert!(relay::verify(&b, &sign(&kp, &b), &keys).is_err(), "{bad}");
    }
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
    let birth = lightbulb::os::process(me).unwrap().birth_micros;
    let pin = ("LIGHTBULB_CONNECT_HOOK_CALLER", format!("{me}:{birth}"));
    a.daemon(
        false,
        &format!("issuer = \"{a_issuer}\"\n"),
        &[trusts(&kp), peers(&[&b]), pin],
    );
    until(10, "both online", || online(&a) && online(&b));
    let got = |e: &Env| e.ask(json!({"id": 1, "op": "relay.get"}));
    until(10, "A fetched ours", || got(&a)["source"] == "lightbulb");
    assert_eq!(got(&a)["urls"], json!([canon(ours)]));
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
    // In effect at once: `relay.get` and a ticket minted right away.
    assert_eq!(got(&a)["source"], "config");
    let t = a.ask(json!({"id": 1, "op": "team.invite"}));
    let ticket = lightbulb::ticket::decode(t["ticket"].as_str().unwrap()).unwrap();
    assert_eq!(ticket.relay_hint, canon(&team_relay));
    until(10, "B adopts the team's relay", || {
        got(&b)["source"] == "team"
    });
    assert_eq!(got(&b)["urls"], json!([canon(&team_relay)]));
    until(30, "B reaches the team's relay", || {
        b.status()["relay"] == "ok"
    });
    // A teammate's ticket carries the relay in effect.
    let t = b.ask(json!({"id": 1, "op": "team.invite"}));
    let ticket = lightbulb::ticket::decode(t["ticket"].as_str().unwrap()).unwrap();
    assert_eq!(ticket.relay_hint, canon(&team_relay));
    assert_eq!(b_hits.load(Ordering::SeqCst), 0, "B contacted our issuer");
    // `--local`: the founder's own relay only; the team keeps its relay.
    let mine = "http://127.0.0.1:2";
    let r = person(
        &a,
        json!({"id": 1, "op": "relay.set", "url": mine, "local": true}),
    );
    assert_eq!(r["team"], false, "{r}");
    assert_eq!(got(&a)["urls"], json!([canon(mine)]));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(got(&b)["urls"], json!([canon(&team_relay)]));

    // With a team relay, even a stale cache contacts nothing of ours.
    let roster = lightbulb::roster::replay(&lightbulb::roster::entries(&b.store()).unwrap());
    let stale = tempfile::tempdir().unwrap();
    let keys = [kp.pk.to_base64()];
    let mut s = relay::State::load(stale.path(), &keys);
    assert!(!s.fetch_if_due(stale.path(), &b_issuer, (&None, &roster.relay), &keys));
    let l = relay::list(&None, &roster.relay, &s);
    assert_eq!(
        (l.source, l.urls),
        (
            relay::Source::Team,
            vec![relay::parse(&team_relay).unwrap()]
        )
    );
    assert_eq!(
        b_hits.load(Ordering::SeqCst),
        0,
        "a team relay contacts no issuer"
    );

    // `relay` alone prints the list and its source.
    let o = a.detached(&[EXE, "relay"], "");
    assert_eq!(out(&o), format!("config  {}\n", canon(mine)));
    // An agent may not move the team's traffic.
    let r: Value = a.ask(json!({"id": 1, "op": "relay.set", "url": "http://x"}));
    assert_eq!(r["reason"], "not_a_person", "{r}");
}

#[test]
fn an_older_list_or_a_tampered_cache_is_refused() {
    let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let keys = vec![kp.pk.to_base64()];
    let newer = list_at(&["https://new.example"], 4_000_000_000);
    let older = list_at(&["https://old.example"], 3_000_000_000);
    let (iss, hits) = issuer(older.clone(), sign(&kp, &older));
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("relays.json"), &newer).unwrap();
    std::fs::write(d.join("relays.json.minisig"), sign(&kp, &newer)).unwrap();
    age(d);
    // The issuer serves an older (validly signed) list: refused, the cached one serves.
    let l = ours_after_fetch(d, &iss, &keys);
    assert!(hits.load(Ordering::SeqCst) >= 1);
    assert_eq!(l.urls, [relay::parse("https://new.example").unwrap()]);
    assert!(
        l.error.as_deref().is_some_and(|e| e.contains("older")),
        "{l:?}"
    );
    // A tampered cache does not verify, and does not lower the floor: the older list is
    // still refused, and nothing of ours is in effect (the default serves).
    std::fs::write(d.join("relays.json"), &older).unwrap();
    age(d);
    let l = ours_after_fetch(d, &iss, &keys);
    assert_eq!(l.source, relay::Source::Default, "{l:?}");
    assert!(l.error.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiner_reaches_the_inviter_through_the_relay_hint_alone() {
    let (_server, url) = relay_server(0).await;
    let (a, c) = (Env::new(), Env::new());
    // A uses the relay; C knows no address of A's and has no relay of its own.
    a.daemon(
        false,
        &format!("relay = \"{url}\"\n"),
        &[("LIGHTBULB_CONNECT_DIALOG", "Allow".into())],
    );
    until(30, "A's relay is up", || a.status()["relay"] == "ok");
    let r = a.ask(json!({"id": 1, "op": "team.invite"}));
    let t = r["ticket"].as_str().unwrap().to_string();
    assert_eq!(
        lightbulb::ticket::decode(&t).unwrap().relay_hint,
        canon(&url)
    );
    c.daemon(false, "", &[relay_only()]);
    assert_eq!(c.status()["relay"], "none");
    let r = person(
        &c,
        json!({"id": 1, "op": "team.join", "ticket": t, "name": "carol", "kind": "computer"}),
    );
    assert_eq!(r["state"], "admitted", "{r}");
}

/// An issuer that answers 404 to everything: no list of ours published.
fn issuer_404() -> String {
    issuer_missing(|_| true)
}

/// An issuer that answers 404 to the request lines `missing` picks, else an empty 200.
fn issuer_missing(missing: fn(&str) -> bool) -> String {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let url = format!("http://{}/connect", l.local_addr().unwrap());
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut r = BufReader::new(&s);
            let mut line = String::new();
            r.read_line(&mut line).unwrap_or_default();
            while r.read_line(&mut String::new()).is_ok_and(|n| n > 2) {}
            let status = if missing(&line) {
                "404 Not Found"
            } else {
                "200 OK"
            };
            let mut s = &s;
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
        }
    });
    url
}

/// The hint a ticket carries with `list` in effect.
fn hint(list: relay::List) -> String {
    let p = lightbulb::net::Peers::default();
    *p.relays.lock().unwrap() = list;
    p.relay_hint()
}

#[test]
fn default_relays_are_used_when_nothing_is_configured() {
    // Nothing set and nothing of ours: the iroh project's public relays, from the iroh crate.
    let l = relay::list(&None, &None, &relay::State::default());
    assert_eq!(l.source, relay::Source::Default);
    assert_eq!(l.urls, relay::default_urls());
    assert!(!l.urls.is_empty());
    assert!(l.urls.iter().all(|u| u
        .host_str()
        .is_some_and(|h| h.ends_with("relay.n0.iroh.link."))));
    // A ticket carries no hint for them: the joiner has the default too.
    assert_eq!(hint(l), "");
    // No list of ours published (404): the default, and nothing to report.
    let dir = tempfile::tempdir().unwrap();
    let l = ours_after_fetch(dir.path(), &issuer_404(), &[]);
    assert_eq!((l.source, l.error), (relay::Source::Default, None));
    // The list without its signature (404 on the .minisig only) is our mistake: reported.
    let dir = tempfile::tempdir().unwrap();
    let iss = issuer_missing(|line| line.contains("relays.json.minisig"));
    let l = ours_after_fetch(dir.path(), &iss, &[]);
    assert_eq!(l.source, relay::Source::Default);
    assert!(l.error.is_some_and(|e| e.contains("404")));
}

#[test]
fn team_relay_overrides_the_default() {
    let team = Some("https://team.example".to_string());
    let l = relay::list(&None, &team, &relay::State::default());
    assert_eq!(l.source, relay::Source::Team);
    assert_eq!(l.urls, [relay::parse("https://team.example").unwrap()]);
    // The founder's relay travels in the ticket's hint.
    assert_eq!(hint(l), canon("https://team.example"));
    let mine = Some("https://mine.example".to_string());
    assert_eq!(
        relay::list(&mine, &team, &relay::State::default()).source,
        relay::Source::Config
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_clear_returns_to_the_default() {
    let (_server, url) = relay_server(0).await;
    let e = Env::new();
    // The default on: the knob that keeps the other tests off the internet is cleared.
    let on = ("LIGHTBULB_CONNECT_DEBUG_NO_DEFAULT_RELAYS", "0".to_string());
    let config = format!("relay = \"{url}\"\nissuer = \"{}\"\n", issuer_404());
    e.daemon(false, &config, &[on]);
    let o = e.detached(&[EXE, "relay"], "");
    assert_eq!(out(&o), format!("config  {}\n", canon(&url)));
    let o = e.detached(&[EXE, "relay", "--clear", "--local"], "");
    assert_eq!(
        out(&o),
        "Relay cleared for this computer. Back to the default: the iroh project's public relays.\n"
    );
    let o = e.detached(&[EXE, "relay"], "");
    assert_eq!(
        out(&o),
        "default  the iroh project's public relays (aps1-1.relay.n0.iroh.link, \
         euc1-1.relay.n0.iroh.link, use1-1.relay.n0.iroh.link, usw1-1.relay.n0.iroh.link)\n\
         Set your own: lightbulb relay <url>\n"
    );
    let o = e.detached(&[EXE, "doctor"], "");
    assert!(
        out(&o).contains("\nRelay                default\n"),
        "{}",
        out(&o)
    );
    e.ask(json!({"id": 1, "op": "shutdown"}));
}
