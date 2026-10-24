//! Relay fallback: two computers that cannot reach each other directly talk through a relay
//! (iroh's own relay server, run here over plain HTTP on the loopback); a team's own relay,
//! which replaces the default; and nothing of ours is ever fetched.

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

/// `url` as the engine writes it (`RelayUrl`'s form: a trailing `/`).
fn canon(url: &str) -> String {
    relay::parse(url).unwrap().to_string()
}

/// A loopback HTTP server that answers 404 to everything, counting the requests it gets: an
/// `issuer` nothing may contact.
fn counting_server() -> (String, Arc<AtomicUsize>) {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let url = format!("http://{}/connect", l.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            h.fetch_add(1, Ordering::SeqCst);
            let mut r = BufReader::new(&s);
            while r.read_line(&mut String::new()).is_ok_and(|n| n > 2) {}
            let mut s = &s;
            let _ = write!(
                s,
                "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn team_relay_from_meta_replaces_the_default() {
    let (_server, team_relay) = relay_server(0).await;
    let (issuer, hits) = counting_server();
    let iss = format!("issuer = \"{issuer}\"\n");
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    b.daemon(false, &iss, &[]);
    // A's daemon takes this test process for an agent (a person's requests come through
    // `person`).
    let me = std::process::id();
    let birth = lightbulb::os::process(me).unwrap().birth_micros;
    let pin = ("LIGHTBULB_CONNECT_HOOK_CALLER", format!("{me}:{birth}"));
    a.daemon(false, &iss, &[peers(&[&b]), pin]);
    until(10, "both online", || online(&a) && online(&b));
    let got = |e: &Env| e.ask(json!({"id": 1, "op": "relay.get"}));
    // Nothing set (and the tests' knob keeps the public relays away): direct only.
    assert_eq!(got(&a)["source"], "none");

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

    let roster = lightbulb::roster::replay(&lightbulb::roster::entries(&b.store()).unwrap());
    let l = relay::list(&None, &roster.relay);
    assert_eq!(
        (l.source, l.urls),
        (
            relay::Source::Team,
            vec![relay::parse(&team_relay).unwrap()]
        )
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0, "the issuer was contacted");

    // `relay` alone prints the list and its source.
    let o = a.detached(&[EXE, "relay"], "");
    assert_eq!(out(&o), format!("config  {}\n", canon(mine)));
    // An agent may not move the team's traffic.
    let r: Value = a.ask(json!({"id": 1, "op": "relay.set", "url": "http://x"}));
    assert_eq!(r["reason"], "not_a_person", "{r}");
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

/// The hint a ticket carries with `list` in effect.
fn hint(list: relay::List) -> String {
    let p = lightbulb::net::Peers::default();
    *p.relays.lock().unwrap() = list;
    p.relay_hint()
}

#[test]
fn default_relays_are_used_when_nothing_is_configured() {
    // Nothing set: the iroh project's public relays, from the iroh crate.
    let l = relay::list(&None, &None);
    assert_eq!(l.source, relay::Source::Default);
    assert_eq!(l.urls, relay::default_urls());
    assert!(!l.urls.is_empty());
    assert!(l.urls.iter().all(|u| u
        .host_str()
        .is_some_and(|h| h.ends_with("relay.n0.iroh.link."))));
    // A ticket carries no hint for them: the joiner has the default too.
    assert_eq!(hint(l), "");
}

#[test]
fn team_relay_overrides_the_default() {
    let team = Some("https://team.example".to_string());
    let l = relay::list(&None, &team);
    assert_eq!(l.source, relay::Source::Team);
    assert_eq!(l.urls, [relay::parse("https://team.example").unwrap()]);
    // The founder's relay travels in the ticket's hint.
    assert_eq!(hint(l), canon("https://team.example"));
    let mine = Some("https://mine.example".to_string());
    assert_eq!(relay::list(&mine, &team).source, relay::Source::Config);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_clear_returns_to_the_default() {
    let (_server, url) = relay_server(0).await;
    let e = Env::new();
    // The default on: the knob that keeps the other tests off the internet is cleared.
    let on = ("LIGHTBULB_CONNECT_DEBUG_NO_DEFAULT_RELAYS", "0".to_string());
    let (issuer, hits) = counting_server();
    let config = format!("relay = \"{url}\"\nissuer = \"{issuer}\"\n");
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
    // With no relay set the daemon calls home for nothing (there was once a daily list).
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(hits.load(Ordering::SeqCst), 0, "the issuer was contacted");
    e.ask(json!({"id": 1, "op": "shutdown"}));
}
