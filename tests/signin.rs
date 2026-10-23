//! Sign-in proofs: the device flow against a loopback test issuer, renewal, a person's second
//! computer admitted by their first, and the workspace member admitted with a workspace proof.

mod common;

use common::issuer::{computer_proof, workspace_proof};
use common::{computer, dialog, err, now_s, out, peers, person, seed_pair, until, Env, EXE};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// What the test issuer was asked: `(path, body)`.
type Asked = Arc<Mutex<Vec<(String, Value)>>>;

/// A plain HTTP/1.1 issuer on the loopback: `device/start` (code `D1`, interval 0),
/// `device/token` (pending once, then a computer proof for the started key as `user_1`
/// "Prasiddha"), `proof/refresh` (a new proof, issued now). Its `issuer` URL.
fn issuer() -> (String, Asked) {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let url = format!("http://{}/connect", l.local_addr().unwrap());
    let asked: Asked = Arc::default();
    let log = asked.clone();
    std::thread::spawn(move || {
        let mut key = String::new();
        let mut polls = 0;
        for s in l.incoming().flatten() {
            let mut r = BufReader::new(&s);
            let mut line = String::new();
            r.read_line(&mut line).unwrap_or_default();
            let mut len = 0;
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) <= 2 {
                    break;
                }
                if let Some(v) = h.to_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap_or_default();
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let path = line.split(' ').nth(1).unwrap_or_default().to_string();
            log.lock().unwrap().push((path.clone(), body.clone()));
            let proof = |k: &str| computer_proof(k, "user_1", "Prasiddha", None, now_s());
            let (status, answer) = if path.ends_with("/device/start") {
                key = body["member_key"].as_str().unwrap_or_default().into();
                let a = json!({"device_code": "D1", "user_code": "ABCD-EFGH",
                    "verification_uri_complete": "https://example.invalid/d?c=ABCD", "interval": 0});
                ("200 OK", a)
            } else if path.ends_with("/device/token") {
                polls += 1;
                match polls {
                    1 => ("200 OK", json!({"pending": true})),
                    2 => ("200 OK", json!({"proof": proof(&key)})),
                    _ => ("410 Gone", json!({})),
                }
            } else if path.ends_with("/proof/refresh") {
                ("200 OK", json!({"proof": proof(&key)}))
            } else {
                ("404 Not Found", json!({}))
            };
            let a = answer.to_string();
            let mut s = &s;
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{a}",
                a.len()
            );
        }
    });
    (url, asked)
}

fn identity_of(e: &Env) -> Value {
    e.ask(json!({"id": 1, "op": "identity.get"}))
}

/// `e` signs in as `sub` with a proof minted here (`identity.proof`).
fn sign_in(e: &Env, sub: &str, ws: Option<(&str, &str)>) {
    let token = computer_proof(&e.identity().key, sub, "Prasiddha", ws, now_s());
    let r = e.ask(json!({"id": 1, "op": "identity.proof", "proof": token}));
    assert_eq!(r["ok"], true, "{r}");
}

/// A fake `osascript` that notes each dialog's text in `dir/texts` and answers `Allow`.
fn allowing(dir: &Path) -> (&'static str, String) {
    let d = dir.display();
    let p = common::script(
        dir,
        &format!(
            "while [ \"$1\" != -- ]; do shift; done\nprintf '%s\\n' \"$3\" >>{d}/texts\n\
             echo 'button returned:Allow, gave up:false'"
        ),
    );
    ("LIGHTBULB_CONNECT_DEBUG_OSASCRIPT", p.display().to_string())
}

fn texts(dir: &Path) -> Vec<String> {
    let t = std::fs::read_to_string(dir.join("texts")).unwrap_or_default();
    t.lines().map(String::from).collect()
}

fn invite(e: &Env) -> String {
    let r = e.ask(json!({"id": 1, "op": "team.invite"}));
    r["ticket"].as_str().unwrap_or_else(|| panic!("{r}")).into()
}

fn members(e: &Env) -> Vec<Value> {
    let r = e.ask(json!({"id": 1, "op": "team.get"}));
    r["members"].as_array().unwrap().clone()
}

fn member<'a>(ms: &'a [Value], key: &str) -> &'a Value {
    ms.iter()
        .find(|m| m["key"] == key)
        .unwrap_or_else(|| panic!("{ms:?}"))
}

#[test]
fn signin_writes_a_merge_and_renews_before_expiry() {
    let (url, asked) = issuer();
    let e = Env::new();
    // The 60 s timer, every second.
    let fast = ("LIGHTBULB_CONNECT_DEBUG_OUTBOX_S", "1".to_string());
    e.daemon(false, &format!("issuer = \"{url}\"\n"), &[fast]);
    let me = e.identity().key;
    let o = e.detached(&[EXE, "signin"], "");
    assert!(o.status.success(), "{}{}", out(&o), err(&o));
    assert_eq!(
        out(&o),
        "Confirm the code ABCD-EFGH in your browser: https://example.invalid/d?c=ABCD\n\
         Signed in as Prasiddha. To join your network, paste its join line: lightbulb-connect join '<link>'\n"
    );
    let id = identity_of(&e);
    assert_eq!(
        (&id["person_id"], &id["verified"], &id["name"]),
        (&json!("user_1"), &json!(true), &json!("Prasiddha")),
        "{id}"
    );
    let starts = asked.lock().unwrap()[0].clone();
    assert_eq!(starts.0, "/connect/device/start");
    assert_eq!(starts.1["member_key"], me);
    // A proof 21 hours old is renewed on the timer, signed by this computer over {proof, at}.
    let old = computer_proof(&me, "user_1", "Prasiddha", None, now_s() - 21 * 3600);
    let r = e.ask(json!({"id": 1, "op": "identity.proof", "proof": old}));
    assert_eq!(r["ok"], true, "{r}");
    until(10, "renewed", || {
        identity_of(&e)["expires_at"].as_i64().unwrap() > now_s() + 23 * 3600
    });
    let asked = asked.lock().unwrap();
    let (path, body) = asked.iter().find(|(p, _)| p.ends_with("refresh")).unwrap();
    assert_eq!(path, "/connect/proof/refresh");
    assert_eq!(body["proof"], json!(old));
    let sig = body["sig"].as_str().unwrap();
    assert!(lightbulb_connect::identity::verify(&me, body, sig));
    assert_eq!(identity_of(&e)["person_id"], "user_1");
}

#[test]
fn a_proof_for_another_computer_or_person_is_refused() {
    let e = Env::new();
    e.daemon(false, "", &[]);
    let other = common::identity();
    let r = e.ask(json!({"id": 1, "op": "identity.proof",
        "proof": computer_proof(&other.key, "user_1", "P", None, now_s())}));
    assert_eq!(r["error"], "bad_request", "{r}");
    sign_in(&e, "user_1", None);
    let r = e.ask(json!({"id": 1, "op": "identity.proof",
        "proof": computer_proof(&e.identity().key, "user_2", "P", None, now_s())}));
    assert_eq!(r["error"], "bad_request", "{r}");
    assert_eq!(identity_of(&e)["person_id"], "user_1");
}

#[test]
fn second_computer_of_same_person_is_admitted_by_the_first() {
    // F founds; A is bob's first computer (signed in as user_bob); B is bob's new Mac.
    let (f, a, b, c) = (Env::new(), Env::new(), Env::new(), Env::new());
    seed_pair(&f, &a);
    let dir = tempfile::tempdir().unwrap();
    a.daemon(false, "", &[allowing(dir.path())]);
    // F's own person would decline: only A may answer.
    f.daemon(false, "", &[peers(&[&a]), dialog("Decline")]);
    until(10, "F and A online", || f.status()["peers"]["online"] == 1);
    sign_in(&a, "user_bob", None);
    let t = invite(&f);
    b.daemon(false, &computer("mbp"), &[peers(&[&f])]);
    sign_in(&b, "user_bob", None);
    // `bob` is A's name: the same person's computer may go by it.
    let o = b.detached(&[EXE, "join", &t, "--name", "bob"], "");
    assert!(o.status.success(), "{}{}", out(&o), err(&o));
    assert_eq!(
        texts(dir.path()),
        ["Your new Mac mbp wants to join. Allow?"]
    );
    let ms = members(&f);
    let mb = member(&ms, &b.identity().key);
    assert_eq!(
        (&mb["person_id"], &mb["name"]),
        (&json!("user_bob"), &json!("bob"))
    );
    assert_eq!(mb["proof"]["sub"], "user_bob");
    // Both of bob's computers offline: a third waits for one of them.
    for e in [&a, &b] {
        e.ask(json!({"id": 1, "op": "shutdown"}));
    }
    until(10, "F sees them gone", || {
        f.status()["peers"]["online"] == 0
    });
    c.daemon(false, &computer("air"), &[peers(&[&f])]);
    sign_in(&c, "user_bob", None);
    let o = c.detached(&[EXE, "join", &t, "--name", "bob"], "");
    assert!(!o.status.success());
    assert_eq!(
        err(&o),
        "Open your other computer, then run the same command again.\n"
    );
    assert_eq!(members(&f).len(), 3);
}

/// F founds a solo network whose dialogs `allowing` answers; W, a solo engine, is to be the
/// workspace member.
fn founder_and_workspace() -> (Env, Env, tempfile::TempDir, String) {
    let (f, w) = (Env::new(), Env::new());
    let dir = tempfile::tempdir().unwrap();
    f.daemon(false, &computer("mac"), &[allowing(dir.path())]);
    let t = invite(&f);
    w.daemon(false, &computer("cloud"), &[peers(&[&f])]);
    (f, w, dir, t)
}

#[test]
fn workspace_member_joins_with_a_workspace_proof() {
    let (f, w, dir, t) = founder_and_workspace();
    let wk = w.identity().key;
    let proof = workspace_proof(
        &wk,
        "W1",
        "The Lightbulb Company",
        &["Friday", "Engineering"],
        now_s(),
    );
    let o = w.detached(&[EXE, "join", &t, "--kind", "cloud", "--proof", &proof], "");
    assert!(o.status.success(), "{}{}", out(&o), err(&o));
    assert_eq!(
        texts(dir.path()),
        ["Lightbulb workspace \"The Lightbulb Company\" wants to join as a member. Its agents (Friday, Engineering) will appear in your directory, and it will see this network's directory."]
    );
    let ms = members(&f);
    let m = member(&ms, &wk);
    assert_eq!(
        (&m["person_id"], &m["name"], &m["kind"], &m["workspace"]),
        (
            &json!("ws:W1"),
            &json!("The Lightbulb Company"),
            &json!("cloud"),
            &json!("W1")
        )
    );
}

#[test]
#[ignore = "lands with the Lane B merge"]
fn second_member_for_one_workspace_is_refused() {
    let (f, w, _dir, t) = founder_and_workspace();
    let proof = |k: &str| workspace_proof(k, "W1", "The Lightbulb Company", &[], now_s());
    let o = w.detached(
        &[
            EXE,
            "join",
            &t,
            "--kind",
            "cloud",
            "--proof",
            &proof(&w.identity().key),
        ],
        "",
    );
    assert!(o.status.success(), "{}", err(&o));
    let w2 = Env::new();
    w2.daemon(false, &computer("cloud2"), &[peers(&[&f])]);
    let r = person(
        &w2,
        json!({"id": 1, "op": "team.join", "ticket": t, "kind": "cloud",
               "proof": proof(&w2.identity().key)}),
    );
    assert_eq!(
        (&r["state"], &r["reason"]),
        (&json!("refused"), &json!("name_taken")),
        "{r}"
    );
}

#[test]
fn computer_proof_cannot_join_as_cloud() {
    let (f, w, dir, t) = founder_and_workspace();
    let ws = Some(("W1", "The Lightbulb Company"));
    let proof = computer_proof(&w.identity().key, "user_1", "Prasiddha", ws, now_s());
    let r = person(
        &w,
        json!({"id": 1, "op": "team.join", "ticket": t, "kind": "cloud", "proof": proof}),
    );
    assert_eq!(
        (&r["state"], &r["reason"]),
        (&json!("refused"), &json!("unsupported")),
        "{r}"
    );
    assert!(texts(dir.path()).is_empty(), "the person was asked");
    assert_eq!(members(&f).len(), 1);
}

#[test]
fn unlink_is_unsupported() {
    let e = Env::new();
    e.daemon(false, "", &[]);
    let r = person(&e, json!({"id": 1, "op": "team.unlink", "member": "x"}));
    assert_eq!(r["error"], "unsupported", "{r}");
}
