//! `lightbulb login` / `logout` against a stub Lightbulb API on loopback: the challenge is
//! signed by this computer's key, the code is shown, the poll ends at the person's confirm.

mod common;

use common::Env;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

/// Calls the stub got: `(path, body)`.
type Seen = Arc<Mutex<Vec<(String, Value)>>>;

/// A stub API answering each request with `answer(path, body, n)` (`n`: that path's count so
/// far): `(status, json)`. Its base URL, and what it saw.
fn stub(answer: fn(&str, &Value, usize) -> (u16, Value)) -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let mut s = s.unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut first = String::new();
            r.read_line(&mut first).unwrap();
            let path = first.split(' ').nth(1).unwrap_or_default().to_string();
            let mut len = 0;
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h == "\r\n" {
                    break;
                }
                if let Some(v) = h.to_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let n = {
                let mut l = log.lock().unwrap();
                let n = l.iter().filter(|(p, _)| *p == path).count();
                l.push((path.clone(), body.clone()));
                n
            };
            let (code, v) = answer(&path, &body, n);
            let text = v.to_string();
            let _ = write!(
                s,
                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
        }
    });
    (base, seen)
}

fn run(e: &Env, base: &str, args: &[&str]) -> Output {
    let mut c = e.base_cmd(Command::new(common::EXE));
    c.args(args)
        .env("LIGHTBULB_API", base)
        .env("LIGHTBULB_CONNECT_NO_BROWSER", "1");
    c.output().unwrap()
}

fn api(path: &str, _body: &Value, n: usize) -> (u16, Value) {
    match (path, n) {
        ("/network/device/challenge", _) => (200, json!({"challenge": format!("c{n}")})),
        ("/network/device/start", _) => (
            200,
            json!({"code": "secret-code", "userCode": "ABCD-EFGH", "interval": 1,
                "expiresIn": 60, "verifyUrl": "https://example.test/auth/network"}),
        ),
        ("/network/device/poll", 0) => (200, json!({"status": "pending"})),
        ("/network/device/poll", _) => (
            200,
            json!({"status": "approved", "name": "Prasiddha", "proof": "h.p.s",
                "routeUrl": "wss://example.test/api/v1/gateway/network"}),
        ),
        _ => (404, json!({"detail": "Not Found"})),
    }
}

#[test]
fn login_signs_the_challenge_shows_the_code_and_waits_for_the_confirm() {
    let e = Env::new();
    let me = e.identity();
    let (base, seen) = stub(api);
    let out = run(&e, &base, &["login"]);
    let text = common::out(&out);
    assert!(out.status.success(), "{text}{}", common::err(&out));
    assert!(text.contains("ABCD-EFGH"), "{text}");
    assert!(text.contains("https://example.test/auth/network"));
    assert!(text.contains("signed in as Prasiddha"), "{text}");
    // The proof and the route link's URL are kept, owner-only (a credential).
    let path = e.state.path().join("login.json");
    let kept: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(kept["proof"], "h.p.s");
    assert_eq!(
        kept["route_url"],
        "wss://example.test/api/v1/gateway/network"
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        !text.contains("secret-code"),
        "the poll code is never shown"
    );
    let seen = seen.lock().unwrap();
    let (_, start) = seen
        .iter()
        .find(|(p, _)| p == "/network/device/start")
        .unwrap();
    assert_eq!(start["purpose"], "login");
    assert_eq!(start["key"], me.key.as_str());
    assert_eq!(start["challenge"], "c0");
    assert!(start["computer"].is_string());
    let signed = json!({"purpose": "lightbulb-login", "challenge": "c0", "key": me.key});
    let sig = start["sig"].as_str().unwrap();
    assert!(lightbulb::identity::verify(&me.key, &signed, sig));
    let polls: Vec<&Value> = (seen.iter())
        .filter(|(p, _)| p == "/network/device/poll")
        .map(|(_, b)| b)
        .collect();
    assert_eq!(polls, [&json!({"code": "secret-code"}); 2]);
}

#[test]
fn logout_deletes_the_sign_in_and_asks_nothing_of_lightbulb() {
    let e = Env::new();
    e.identity();
    e.sign_in("user_aden", None);
    let (base, seen) = stub(api);
    let out = run(&e, &base, &["logout"]);
    assert!(out.status.success(), "{}", common::err(&out));
    assert!(common::out(&out).contains("signed out"));
    assert!(!e.state.path().join("login.json").exists());
    assert!(seen.lock().unwrap().is_empty(), "no call");
    let again = run(&e, &base, &["logout"]);
    assert!(common::out(&again).contains("was not signed in"));
}

#[test]
fn a_refusal_is_the_backends_sentence_and_no_identity_is_a_setup_hint() {
    fn refuse(path: &str, body: &Value, n: usize) -> (u16, Value) {
        match path {
            "/network/device/start" => (409, json!({"detail": "Lightbulb said no."})),
            _ => api(path, body, n),
        }
    }
    let e = Env::new();
    e.identity();
    let (base, _) = stub(refuse);
    let out = run(&e, &base, &["login"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(common::err(&out).contains("Lightbulb said no."));

    let fresh = Env::new();
    let out = run(&fresh, &base, &["login"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        common::err(&out).contains("lightbulb setup"),
        "{}",
        common::err(&out)
    );
    assert!(!fresh.state.path().join("identity.key").exists());
}

#[test]
fn invite_agents_hands_the_backend_a_once_ask_link_under_this_computers_signature() {
    fn agents(path: &str, body: &Value, n: usize) -> (u16, Value) {
        match path {
            "/network/device/poll" => (200, json!({"status": "approved", "workspaceName": "Acme"})),
            _ => api(path, body, n),
        }
    }
    let e = Env::new();
    e.daemon(false, "", &[]);
    let me = e.identity();
    e.sign_in("user_aden", None);
    let (base, seen) = stub(agents);
    let api_env = format!("LIGHTBULB_API={base}");
    let argv = ["/usr/bin/env", &api_env, "LIGHTBULB_CONNECT_NO_BROWSER=1"];
    let out = e.detached(
        &[&argv[..], &[common::EXE, "invite", "--agents"]].concat(),
        "",
    );
    let text = common::out(&out);
    assert!(out.status.success(), "{text}{}", common::err(&out));
    assert!(text.contains("Acme is joining"), "{text}");
    let seen = seen.lock().unwrap();
    let (_, start) = seen
        .iter()
        .find(|(p, _)| p == "/network/device/start")
        .unwrap();
    assert_eq!(start["purpose"], "agents");
    // Whose computer this is: its sign-in proof rides the start.
    let kept: Value =
        serde_json::from_slice(&std::fs::read(e.state.path().join("login.json")).unwrap()).unwrap();
    assert_eq!(start["proof"], kept["proof"]);
    let ticket = start["ticket"].as_str().unwrap();
    let t = lightbulb::ticket::decode(ticket).unwrap();
    assert_eq!(
        (t.mode.as_str(), t.inviter.as_str()),
        ("ask", me.key.as_str())
    );
    let r = lightbulb::roster::replay(&lightbulb::roster::entries(&e.store()).unwrap());
    assert!(r.links[&t.ticket_id].once, "a once link");
    // No second link behind a four-word code.
    assert_eq!(r.links.len(), 1, "{:?}", r.links.keys());
    let signed = json!({"purpose": "lightbulb-agents", "challenge": "c0", "key": me.key,
        "ticket": ticket});
    assert!(lightbulb::identity::verify(
        &me.key,
        &signed,
        start["sig"].as_str().unwrap()
    ));
}

#[test]
fn an_expired_sign_in_says_sign_in_again_and_its_age_does_not() {
    let e = Env::new();
    let key = e.identity().key;
    let now = common::now_s();
    let ok = common::issuer::mint(&json!({"iat": now, "exp": now + 86_400, "sub": "user_a",
        "computer": key, "auth_time": now - 10 * 86_400}));
    let old = common::issuer::mint(&json!({"iat": now, "exp": now + 86_400, "sub": "user_a",
        "computer": key, "auth_time": now - 91 * 86_400}));
    let expired = common::issuer::mint(&json!({"iat": now - 40 * 86_400, "exp": now - 10 * 86_400,
        "sub": "user_a", "computer": key}));
    let dir = e.state.path();
    assert!(
        !lightbulb::login::ended(dir, now),
        "not signed in: nothing ended"
    );
    // The browser login's age is the account server's to judge at renewal, not ours.
    for (token, ended) in [(ok, false), (old, false), (expired, true)] {
        lightbulb::login::save(dir, &json!({ "proof": token })).unwrap();
        assert_eq!(lightbulb::login::ended(dir, now), ended);
    }
    let out = e.detached(&[common::EXE, "doctor"], "");
    let text = format!("{}{}", common::out(&out), common::err(&out));
    assert!(
        text.contains("Lightbulb sign-in") && text.contains("lightbulb login"),
        "{text}"
    );
}
