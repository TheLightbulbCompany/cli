//! `lightbulb login` / `logout` against a stub Lightbulb API on loopback: the challenge is
//! signed by this computer's key, the code is shown, the poll ends at the person's confirm.

mod common;

use common::Env;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

/// Calls the stub got: `(path, body)`; a signed call adds `("proof <path>", its proof)`.
type Seen = Arc<Mutex<Vec<(String, Value)>>>;

/// A stub API answering each request with `answer(path, body, n)` (`n`: that path's count so
/// far): `(status, json)`. Its base URL, and what it saw.
fn stub(answer: impl Fn(&str, &Value, usize) -> (u16, Value) + Send + 'static) -> (String, Seen) {
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
            let mut proof = None;
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h == "\r\n" {
                    break;
                }
                if let Some(v) = h.to_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                if let Some(v) = h.strip_prefix("X-Lightbulb-Proof:") {
                    proof = Some(v.trim().to_string());
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let n = {
                let mut l = log.lock().unwrap();
                let n = l.iter().filter(|(p, _)| *p == path).count();
                l.push((path.clone(), body.clone()));
                if let Some(p) = proof {
                    l.push((format!("proof {path}"), json!(p)));
                }
                n
            };
            let (code, v) = answer(&path, &body, n);
            let text = v.to_string();
            let _ = write!(
                s,
                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{text}",
                text.len(),
                // A cold member's answer; the wait is short, so a test ends.
                if code == 503 { "Retry-After: 1\r\n" } else { "" }
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
fn logout_signs_this_computer_out_at_lightbulb_then_deletes_the_sign_in() {
    fn revoke(path: &str, body: &Value, n: usize) -> (u16, Value) {
        match path {
            "/network/proof/revoke" => (200, json!({"state": "signed_out"})),
            _ => api(path, body, n),
        }
    }
    let e = Env::new();
    let me = e.identity();
    e.sign_in("user_aden", None);
    let proof = lightbulb::login::stored_proof(e.state.path()).unwrap();
    let (base, seen) = stub(revoke);
    let out = run(&e, &base, &["logout"]);
    assert!(out.status.success(), "{}", common::err(&out));
    assert!(common::out(&out).contains("This computer is signed out."));
    assert!(!e.state.path().join("login.json").exists());
    {
        let seen = seen.lock().unwrap();
        let (_, body) = seen
            .iter()
            .find(|(p, _)| p == "/network/proof/revoke")
            .expect("logout tells Lightbulb");
        assert_eq!(body["proof"], proof.as_str());
        assert_eq!(body["key"], me.key.as_str());
        let signed =
            json!({"purpose": "lightbulb-logout", "challenge": body["challenge"], "key": me.key});
        assert!(lightbulb::identity::verify(
            &me.key,
            &signed,
            body["sig"].as_str().unwrap()
        ));
    }
    let again = run(&e, &base, &["logout"]);
    assert!(common::out(&again).contains("was not signed in"));
    assert_eq!(seen.lock().unwrap().len(), 2, "nothing more to tell");
}

#[test]
fn offline_logout_still_signs_out_here_and_says_lightbulb_was_not_told() {
    let e = Env::new();
    e.identity();
    e.sign_in("user_aden", None);
    let out = run(&e, "http://127.0.0.1:9", &["logout"]);
    assert!(out.status.success(), "{}", common::err(&out));
    let text = common::out(&out);
    assert!(text.contains("Lightbulb was not told"), "{text}");
    assert!(!e.state.path().join("login.json").exists());
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
fn invite_agents_hands_the_backend_a_once_link_under_this_computers_signature() {
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
    assert!(text.contains("Acme is joining this network"), "{text}");
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
        ("open", me.key.as_str())
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

const NOTICE: &str = "Your computer joined Acme's network. Teammates on it see the names, folders \
                      and status of your sessions, never your files or conversations. Hide a \
                      folder: lightbulb folder <path> off.";

/// `login` from outside any agent's process tree, against `base`.
fn login_detached(e: &Env, base: &str, args: &[&str]) -> Output {
    let api_env = format!("LIGHTBULB_API={base}");
    let argv = ["/usr/bin/env", &api_env, "LIGHTBULB_CONNECT_NO_BROWSER=1"];
    e.detached(&[&argv[..], &[common::EXE], args].concat(), "")
}

/// The stub, with `team` answering `GET /network/team`, `line` `POST /network/join-line`.
fn workspace(team: Value, line: (u16, Value)) -> (String, Seen) {
    stub(move |path, body, n| match path {
        "/network/team" => (200, team.clone()),
        "/network/join-line" => line.clone(),
        _ => api(path, body, n),
    })
}

fn acme(team_id: Value) -> Value {
    json!({"workspace": "Acme", "teamId": team_id, "userId": "user_a", "name": "Aden"})
}

fn called(seen: &Seen, path: &str) -> bool {
    seen.lock().unwrap().iter().any(|(p, _)| p == path)
}

#[test]
fn login_app_prints_one_json_line_and_opens_nothing() {
    let e = Env::new();
    e.identity();
    // Openers on the PATH that say they ran. No NO_BROWSER here: nothing else stops them.
    let dir = tempfile::tempdir().unwrap();
    let ran = dir.path().join("ran");
    for opener in ["open", "xdg-open"] {
        let p = dir.path().join(opener);
        std::fs::write(&p, format!("#!/bin/sh\ntouch '{}'\n", ran.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap()
    );
    let (base, seen) = stub(api);
    let login = |args: &[&str]| {
        let mut c = e.base_cmd(Command::new(common::EXE));
        c.arg("login")
            .args(args)
            .env("LIGHTBULB_API", &base)
            .env("PATH", &path)
            .env_remove("LIGHTBULB_CONNECT_NO_BROWSER");
        c.output().unwrap()
    };
    let out = login(&["--app"]);
    assert!(out.status.success(), "{}", common::err(&out));
    assert_eq!(common::out(&out), "{\"user_code\":\"ABCD-EFGH\"}\n");
    assert!(!ran.exists(), "--app opens nothing");
    // It signed in, and asked for no network.
    assert_eq!(
        lightbulb::login::stored_proof(e.state.path()).unwrap(),
        "h.p.s"
    );
    assert!(!called(&seen, "/network/team"));
    // The plain `login` does open it: the openers above are real.
    let out = login(&[]);
    assert!(out.status.success(), "{}", common::err(&out));
    assert!(ran.exists(), "login opens the browser");
    // Hidden from the help.
    let out = run(&e, &base, &["login", "--help"]);
    assert!(
        !common::out(&out).contains("--app"),
        "{}",
        common::out(&out)
    );
}

#[test]
fn a_backend_without_the_routes_is_a_plain_login() {
    let e = Env::new();
    e.identity();
    let (base, seen) = stub(api);
    let out = run(&e, &base, &["login"]);
    let text = common::out(&out);
    assert!(out.status.success(), "{text}{}", common::err(&out));
    assert!(text.ends_with("Not you? Run lightbulb logout.\n"), "{text}");
    assert!(common::err(&out).is_empty(), "{}", common::err(&out));
    // A signed call, as the workspace tools make.
    assert!(called(&seen, "proof /network/team"));
}

#[test]
fn a_person_with_no_workspace_joins_nothing() {
    let e = Env::new();
    e.identity();
    let team = json!({"workspace": null, "teamId": null, "userId": "user_a", "name": "Aden"});
    let (base, seen) = workspace(team, (500, json!({})));
    let out = run(&e, &base, &["login"]);
    let text = common::out(&out);
    assert!(out.status.success(), "{text}");
    assert!(text.ends_with("Not you? Run lightbulb logout.\n"), "{text}");
    assert!(!called(&seen, "/network/join-line"));
}

#[test]
fn a_computer_already_on_the_workspaces_network_is_told_so() {
    let e = Env::new();
    e.daemon(false, "", &[]);
    let id = e.ask(json!({"id": 1, "op": "team.get"}))["team_id"].clone();
    let (base, seen) = workspace(acme(id), (500, json!({})));
    let out = login_detached(&e, &base, &["login"]);
    let text = common::out(&out);
    assert!(out.status.success(), "{text}{}", common::err(&out));
    assert!(text.ends_with("On Acme's network.\n"), "{text}");
    assert!(!called(&seen, "/network/join-line"));
}

#[test]
fn a_computer_on_another_network_is_told_how_and_moved_nowhere() {
    let (a, b) = (Env::new(), Env::new());
    common::seed_pair(&a, &b);
    b.daemon(false, "", &[]);
    let team = b.ask(json!({"id": 1, "op": "team.get"}));
    let (base, seen) = workspace(acme(json!("elsewhere")), (500, json!({})));
    let out = login_detached(&b, &base, &["login"]);
    let text = common::out(&out);
    assert!(out.status.success(), "{text}{}", common::err(&out));
    let want = format!(
        "You are on {}. To join Acme's network: lightbulb leave, then lightbulb login.\n",
        team["name"].as_str().unwrap()
    );
    assert!(text.ends_with(&want), "{text}");
    assert!(!called(&seen, "/network/join-line"));
    assert_eq!(common::members(&b).len(), 2);
}

/// Computer A, online, and its once link as the workspace's line for computer B ("mbp"), alone
/// and reaching A, under the name `name`; B's `login` output.
fn login_joins(name: Option<&str>) -> (Env, Env, Output, Seen) {
    let (a, b) = (Env::new(), Env::new());
    a.daemon(false, &common::computer("mac"), &[]);
    let ticket = common::invite(&a, "once");
    b.daemon(false, &common::computer("mbp"), &[common::peers(&[&a])]);
    let taken = common::members(&a)[0]["name"].as_str().unwrap().to_string();
    let name = name.unwrap_or(&taken);
    let line = json!({"ticket": ticket, "teamId": "t", "workspace": "Acme", "name": name,
        "expiresAt": 0});
    let (base, seen) = workspace(acme(json!("t")), (200, line));
    let out = login_detached(&b, &base, &["login"]);
    (a, b, out, seen)
}

#[test]
fn a_computer_alone_joins_and_gets_the_notice() {
    let (a, b, out, seen) = login_joins(Some("Aden"));
    let text = common::out(&out);
    assert!(out.status.success(), "{text}{}", common::err(&out));
    assert!(
        text.contains("Connecting your computer to Acme…\n"),
        "{text}"
    );
    assert!(text.ends_with(&format!("{NOTICE}\n")), "{text}");
    assert!(called(&seen, "proof /network/join-line"));
    let names: Vec<String> = common::members(&b)
        .iter()
        .map(|m| m["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"Aden".to_string()), "{names:?}");
    assert_eq!(common::members(&a).len(), 2);
    assert!(!text.contains("eyJ"), "the ticket is never shown: {text}");
}

#[test]
fn a_taken_name_joins_as_the_name_and_computer_on_the_same_line() {
    let (a, b, out, seen) = login_joins(None);
    let text = common::out(&out);
    assert!(out.status.success(), "{text}{}", common::err(&out));
    assert!(text.ends_with(&format!("{NOTICE}\n")), "{text}");
    let posts = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p == "/network/join-line")
        .count();
    assert_eq!(posts, 1, "the same line");
    let taken = common::members(&a)
        .iter()
        .map(|m| m["name"].as_str().unwrap().to_string())
        .find(|n| n.ends_with(" (mbp)"));
    assert!(taken.is_some(), "{:?}", common::members(&a));
    assert_eq!(common::members(&b).len(), 2);
}

#[test]
fn a_failed_join_is_one_line_and_the_login_stands() {
    let e = Env::new();
    e.daemon(false, "", &[]);
    let line = (409, json!({"detail": "This workspace has no agents yet."}));
    let (base, _) = workspace(acme(Value::Null), line);
    let out = login_detached(&e, &base, &["login"]);
    let text = common::out(&out);
    assert!(out.status.success(), "{text}{}", common::err(&out));
    assert_eq!(
        lightbulb::login::stored_proof(e.state.path()).unwrap(),
        "h.p.s"
    );
    let last = text.lines().last().unwrap();
    assert!(last.contains("This workspace has no agents yet."), "{text}");
    assert_eq!(common::members(&e).len(), 1);
}

#[test]
fn a_cold_member_is_waited_for_at_its_retry_after() {
    let (a, b) = (Env::new(), Env::new());
    a.daemon(false, &common::computer("mac"), &[]);
    let ticket = common::invite(&a, "once");
    b.daemon(false, &common::computer("mbp"), &[common::peers(&[&a])]);
    let (base, seen) = stub(move |path, body, n| match (path, n) {
        ("/network/team", _) => (200, acme(json!("t"))),
        ("/network/join-line", 0 | 1) => (503, json!({"detail": "Waking."})),
        ("/network/join-line", _) => (200, json!({"ticket": ticket, "name": "Aden"})),
        _ => api(path, body, n),
    });
    let out = login_detached(&b, &base, &["login"]);
    let text = common::out(&out);
    assert!(out.status.success(), "{text}{}", common::err(&out));
    assert!(text.ends_with(&format!("{NOTICE}\n")), "{text}");
    assert_eq!(text.matches("Connecting").count(), 1, "{text}");
    let posts = (seen.lock().unwrap().iter())
        .filter(|(p, _)| p == "/network/join-line")
        .count();
    assert_eq!(posts, 3);
}

#[test]
fn a_member_that_never_wakes_is_one_line_and_the_login_stands() {
    let e = Env::new();
    e.daemon(false, "", &[]);
    let (base, seen) = workspace(acme(Value::Null), (503, json!({"detail": "Waking."})));
    let api_env = format!("LIGHTBULB_API={base}");
    let argv = [
        "/usr/bin/env",
        &api_env,
        "LIGHTBULB_CONNECT_NO_BROWSER=1",
        "LIGHTBULB_CONNECT_JOIN_WAIT_S=2",
        common::EXE,
        "login",
    ];
    let out = e.detached(&argv, "");
    let text = common::out(&out);
    assert!(out.status.success(), "{text}{}", common::err(&out));
    assert!(
        text.lines().last().unwrap().contains("did not wake"),
        "{text}"
    );
    assert_eq!(
        lightbulb::login::stored_proof(e.state.path()).unwrap(),
        "h.p.s"
    );
    let posts = (seen.lock().unwrap().iter())
        .filter(|(p, _)| p == "/network/join-line")
        .count();
    assert!(posts >= 2, "{posts}");
}

#[test]
fn a_network_nobody_answers_is_not_called_a_refusal() {
    let (a, b) = (Env::new(), Env::new());
    let id = a.identity();
    let now = common::now_s();
    let g = lightbulb::daemon::net_ops::genesis(&id, "aden", "mac", "acme", now);
    lightbulb::roster::accept(&a.store(), &g, now).unwrap();
    let r = lightbulb::roster::replay(&lightbulb::roster::entries(&a.store()).unwrap());
    // A's ticket, and A never runs.
    let t = lightbulb::ticket::sign(
        &id,
        &r,
        format!("{}.t1", id.short),
        "s".into(),
        "open",
        now + 60,
        (String::new(), vec![]),
        None,
    );
    let ticket = lightbulb::ticket::encode(&t);
    b.daemon(
        false,
        "",
        &[("LIGHTBULB_CONNECT_DEBUG_DIAL_WAIT_S", "1".into())],
    );
    let line = json!({"ticket": ticket, "name": "Aden"});
    let (base, _) = workspace(acme(json!("t")), (200, line));
    let out = login_detached(&b, &base, &["login"]);
    let text = common::out(&out);
    assert!(out.status.success(), "{text}{}", common::err(&out));
    assert!(
        text.ends_with("Couldn't reach your workspace's network; try lightbulb login again.\n"),
        "{text}"
    );
    assert!(!text.contains("refused"), "{text}");
}
