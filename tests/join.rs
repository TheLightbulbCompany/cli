//! Admission: a candidate joins with a ticket; a listed member checks it, asks its person
//! (or admits at once for an open link), writes the `admit` and hands over the roster. Real
//! daemons; a test that needs a hand-built request plays the candidate over its own endpoint.

mod common;

use common::{err, out, seed_pair, until, Env, EXE};
use iroh::endpoint::{presets, Connection, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, PublicKey, SecretKey};
use lightbulb_network::admission;
use lightbulb_network::identity::{self, Identity};
use lightbulb_network::net::frames::{self, Frame};
use lightbulb_network::net::JOIN_ALPN;
use lightbulb_network::roster;
use lightbulb_network::ticket::{self, Ticket};
use serde_json::{json, Value};
use std::time::Duration;

fn dialog(button: &str) -> (&'static str, String) {
    ("LIGHTBULB_NETWORK_DIALOG", button.into())
}

fn peers(envs: &[&Env]) -> (&'static str, String) {
    let list: Vec<String> = envs.iter().map(|e| e.peer_arg()).collect();
    ("LIGHTBULB_NETWORK_PEERS", list.join(","))
}

fn computer(name: &str) -> String {
    format!("computer = \"{name}\"\n")
}

fn invite(e: &Env, mode: &str) -> String {
    let r = e.ask(json!({"id": 1, "op": "team.invite", "mode": mode}));
    assert_eq!(r["ok"], true, "{r}");
    r["ticket"].as_str().unwrap().into()
}

fn join(e: &Env, ticket: &str, name: &str) -> Value {
    e.ask(json!({"id": 1, "op": "team.join", "ticket": ticket, "name": name, "kind": "computer"}))
}

fn refused(r: &Value, reason: &str) {
    assert_eq!(
        (r["state"].as_str(), r["reason"].as_str()),
        (Some("refused"), Some(reason)),
        "{r}"
    );
}

fn members(e: &Env) -> usize {
    e.ask(json!({"id": 1, "op": "team.get"}))["members"]
        .as_array()
        .unwrap()
        .len()
}

fn requests(e: &Env) -> Vec<Value> {
    let r = e.ask(json!({"id": 1, "op": "team.join_requests"}));
    r["requests"]
        .as_array()
        .unwrap_or_else(|| panic!("{r}"))
        .clone()
}

/// `req` from a person: a detached `nc`.
fn person(e: &Env, req: Value) -> Value {
    let sock = e.state.path().join("sock");
    let nc = e.detached(
        &["nc", "-w", "1", "-U", sock.to_str().unwrap()],
        &format!("{req}\n"),
    );
    serde_json::from_slice(&nc.stdout).unwrap()
}

fn identity() -> Identity {
    Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap()
}

fn addr(env: &Env) -> EndpointAddr {
    let key = PublicKey::from_bytes(&identity::key_bytes(&env.identity().key).unwrap()).unwrap();
    let port = env.status()["port"].as_u64().unwrap();
    EndpointAddr::new(key).with_ip_addr(format!("127.0.0.1:{port}").parse().unwrap())
}

/// A candidate played by the test over its own endpoint (holding `as_key`'s key), having sent
/// `request` to `env`'s daemon on the join ALPN.
struct Played {
    _ep: Endpoint,
    conn: Connection,
    _send: SendStream,
    recv: RecvStream,
}

impl Played {
    async fn send(env: &Env, as_key: &Identity, request: &Frame) -> Played {
        let ep = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::from_bytes(&as_key.signing.to_bytes()))
            .bind()
            .await
            .unwrap();
        let conn = ep.connect(addr(env), JOIN_ALPN).await.unwrap();
        let (mut send, recv) = conn.open_bi().await.unwrap();
        frames::write(&mut send, request).await.unwrap();
        Played {
            _ep: ep,
            conn,
            _send: send,
            recv,
        }
    }

    /// A candidate of its own key asking to join as `name` with `t`.
    async fn ask(env: &Env, t: &Ticket, name: &str) -> Played {
        let id = identity();
        Played::send(
            env,
            &id,
            &admission::request(&id, t, name, "mbp", "computer"),
        )
        .await
    }

    async fn recv(&mut self) -> std::result::Result<Frame, String> {
        tokio::time::timeout(Duration::from_secs(5), frames::read(&mut self.recv))
            .await
            .map_err(|_| "no frame within 5 s".to_string())?
            .map_err(|e| e.to_string())
    }
}

#[test]
fn ask_ticket_admits_only_after_allow() {
    let (a, b) = (Env::new(), Env::new());
    a.daemon(false, &computer("mac"), &[dialog("")]);
    let t = invite(&a, "reusable");
    b.daemon(false, &computer("mbp"), &[peers(&[&a])]);
    // `$USER` is A's name too.
    let o = b.detached(&[EXE, "join", &t], "");
    assert!(!o.status.success());
    let taken = format!(
        "The name \"prasiddha\" is already used in this network. Try: lightbulb-network join '{t}' --name prasiddha-2\n"
    );
    assert_eq!(err(&o), taken);
    std::thread::scope(|s| {
        let joining = s.spawn(|| b.detached(&[EXE, "join", &t, "--name", "bob"], ""));
        until(10, "J1 is listed", || requests(&a).len() == 1);
        let r = &requests(&a)[0];
        assert_eq!(r["id"], "J1");
        assert_eq!(r["candidate"], b.identity().key);
        assert_eq!((&r["name"], &r["computer"]), (&json!("bob"), &json!("mbp")));
        assert_eq!((members(&a), members(&b)), (1, 1));
        let o = a.detached(&[EXE, "approve", "J1"], "");
        assert!(o.status.success(), "{}", err(&o));
        let o = joining.join().unwrap();
        assert!(o.status.success(), "{}", err(&o));
        assert_eq!(
            out(&o),
            "Waiting for an admin to allow this...\nConnected as bob (mbp). Team \"prasiddha\": 2 computers online.\n"
        );
    });
    until(5, "both list 2 members", || {
        members(&a) == 2 && members(&b) == 2
    });
    assert_eq!(a.heads(), b.heads());
    // A second answer finds nothing open.
    let o = a.detached(&[EXE, "decline", "J1"], "");
    assert!(!o.status.success());
    let r = a.detached(&[EXE, "approve", "J9"], "");
    assert!(!r.status.success());
}

#[test]
fn decline_refuses_and_writes_nothing() {
    let (a, b) = (Env::new(), Env::new());
    a.daemon(false, "", &[dialog("Decline")]);
    let t = invite(&a, "reusable");
    let heads = a.heads();
    b.daemon(false, "", &[peers(&[&a])]);
    refused(&join(&b, &t, "bob"), "declined");
    assert_eq!(a.heads(), heads);
    assert_eq!((members(&a), members(&b)), (1, 1));
    assert!(requests(&a).is_empty());
}

#[test]
fn open_ticket_admits_without_prompt() {
    let (a, b) = (Env::new(), Env::new());
    // A prompt would decline.
    a.daemon(false, "", &[dialog("Decline")]);
    let line = person(&a, json!({"id": 1, "op": "team.invite", "mode": "open"}))["line"]
        .as_str()
        .unwrap()
        .to_string();
    b.daemon(false, "", &[peers(&[&a])]);
    let r = join(&b, &line, "bob");
    assert_eq!(r["state"], "admitted", "{r}");
    assert_eq!(r["team"], "prasiddha");
    until(5, "both list 2 members", || {
        members(&a) == 2 && members(&b) == 2
    });
}

#[test]
fn forged_open_mode_is_refused() {
    let (a, b) = (Env::new(), Env::new());
    a.daemon(false, "", &[dialog("Allow")]);
    let mut t = ticket::decode(&invite(&a, "reusable")).unwrap();
    t.mode = "open".into();
    b.daemon(false, "", &[peers(&[&a])]);
    refused(&join(&b, &ticket::encode(&t), "bob"), "bad_ticket");
    assert_eq!((members(&a), members(&b)), (1, 1));
}

#[test]
fn once_ticket_admits_once() {
    let (f, x, c1, c2) = (Env::new(), Env::new(), Env::new(), Env::new());
    seed_pair(&f, &x);
    x.daemon(false, "", &[dialog("Allow")]);
    f.daemon(false, "", &[peers(&[&x]), dialog("Allow")]);
    let t = invite(&x, "once");
    assert_eq!(
        ticket::decode(&t).unwrap().members,
        [x.identity().key, f.identity().key]
    );
    c1.daemon(false, "", &[peers(&[&x])]);
    assert_eq!(join(&c1, &t, "carol")["state"], "admitted");
    until(5, "F converges", || members(&f) == 3);
    x.ask(json!({"id": 1, "op": "shutdown"}));
    // The inviter is offline: C2 reaches only F.
    c2.daemon(false, "", &[peers(&[&f])]);
    refused(&join(&c2, &t, "dave"), "bad_ticket");
    assert_eq!(members(&f), 3);
}

#[test]
fn forged_ticket_from_another_members_link_is_refused() {
    let (f, x, c) = (Env::new(), Env::new(), Env::new());
    seed_pair(&f, &x);
    x.daemon(false, "", &[dialog("Allow")]);
    f.daemon(false, "", &[peers(&[&x]), dialog("Allow")]);
    let xt = ticket::decode(&invite(&x, "reusable")).unwrap();
    until(5, "F holds X's link", || f.heads() == x.heads());
    // F knows X's link secret from the roster and re-signs it as its own open link.
    let r = roster::replay(&roster::entries(&f.store()).unwrap());
    let forged = ticket::sign(
        &f.identity(),
        &r,
        xt.ticket_id,
        xt.secret,
        "open",
        xt.expires_at,
    );
    c.daemon(false, "", &[peers(&[&f])]);
    refused(&join(&c, &ticket::encode(&forged), "carol"), "bad_ticket");
    assert_eq!((members(&f), members(&x)), (2, 2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn candidate_field_must_be_the_peer_key() {
    let a = Env::new();
    a.daemon(false, "", &[dialog("Allow")]);
    let t = ticket::decode(&invite(&a, "reusable")).unwrap();
    // Signed by `other`, naming `other`, sent over `me`'s endpoint.
    let (me, other) = (identity(), identity());
    let req = admission::request(&other, &t, "carol", "mbp", "computer");
    let mut p = Played::send(&a, &me, &req).await;
    let got = p.recv().await;
    assert!(got.is_err(), "answered: {got:?}");
    assert!(requests(&a).is_empty());
    assert_eq!(members(&a), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sixth_open_request_for_one_inviter_is_refused() {
    let a = Env::new();
    a.daemon(false, "", &[dialog("")]);
    let t = ticket::decode(&invite(&a, "reusable")).unwrap();
    let mut held = vec![];
    for i in 0..5 {
        held.push(Played::ask(&a, &t, &format!("c{i}")).await);
    }
    until(10, "five listed", || requests(&a).len() == 5);
    let mut sixth = Played::ask(&a, &t, "c5").await;
    match sixth.recv().await.unwrap() {
        Frame::JoinDecision {
            verdict, reason, ..
        } => assert_eq!(
            (verdict.as_str(), reason.as_deref()),
            ("refused", Some("too_many_requests"))
        ),
        f => panic!("{f:?}"),
    }
    assert_eq!(requests(&a).len(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_requests_expire_and_free_the_limit() {
    let a = Env::new();
    // The 600 s wait, shortened.
    let wait = ("LIGHTBULB_NETWORK_DEBUG_JOIN_WAIT_S", "3".to_string());
    a.daemon(false, "", &[dialog(""), wait]);
    let t = ticket::decode(&invite(&a, "reusable")).unwrap();
    let mut held = vec![];
    for i in 0..5 {
        held.push(Played::ask(&a, &t, &format!("c{i}")).await);
    }
    until(10, "five listed", || requests(&a).len() == 5);
    // Two candidates hang up; the other three stop being waited for after the window.
    for p in &held[..2] {
        p.conn.close(0u32.into(), b"");
    }
    until(2, "two expired on close", || requests(&a).len() == 3);
    until(10, "the rest expired", || requests(&a).is_empty());
    let states: Vec<String> = rusqlite::Connection::open(a.state.path().join("state.db"))
        .unwrap()
        .prepare("SELECT state FROM join_requests ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(states, ["expired"; 5]);
    let _sixth = Played::ask(&a, &t, "c5").await;
    until(5, "a sixth is accepted", || {
        requests(&a).first().is_some_and(|r| r["id"] == "J6")
    });
}

#[test]
fn link_replaced_on_removal_no_longer_joins() {
    let (a, b, c) = (Env::new(), Env::new(), Env::new());
    seed_pair(&a, &b);
    a.daemon(false, "", &[dialog("Allow")]);
    let t = invite(&a, "reusable");
    let o = a.detached(&[EXE, "remove", "bob"], "");
    assert!(o.status.success(), "{}", err(&o));
    let line = out(&o)
        .split("replaced: ")
        .nth(1)
        .unwrap()
        .trim()
        .to_string();
    c.daemon(false, "", &[peers(&[&a])]);
    refused(&join(&c, &t, "carol"), "bad_ticket");
    assert_eq!(members(&a), 1);
    // The link that replaced it does.
    assert_eq!(join(&c, &line, "carol")["state"], "admitted");
}

#[test]
fn cloud_candidate_without_bridge_proof_is_refused() {
    let (a, b) = (Env::new(), Env::new());
    a.daemon(false, "", &[dialog("Allow")]);
    let t = invite(&a, "reusable");
    b.daemon(false, "", &[peers(&[&a])]);
    let r = b.ask(json!({"id": 1, "op": "team.join", "ticket": t, "name": "bot", "kind": "cloud"}));
    refused(&r, "unsupported");
    assert_eq!(members(&a), 1);
}

#[test]
fn join_from_a_multi_member_network_is_refused() {
    let (a, b, c) = (Env::new(), Env::new(), Env::new());
    seed_pair(&a, &b);
    b.daemon(false, "", &[]);
    c.daemon(false, "", &[dialog("Allow")]);
    let t = invite(&c, "reusable");
    let heads = b.heads();
    let r = join(&b, &t, "bob2");
    refused(&r, "already_in_team");
    assert_eq!(r["team"], "acme");
    let o = b.detached(&[EXE, "join", &t], "");
    assert!(!o.status.success());
    assert_eq!(
        err(&o),
        "You are in \"acme\". Leave it first: lightbulb-network leave\n"
    );
    assert_eq!(b.heads(), heads);
    assert_eq!(members(&c), 1);
}

#[test]
fn non_admin_inviter_admits_its_own_invitee_with_no_admin_online() {
    let (f, x, c) = (Env::new(), Env::new(), Env::new());
    seed_pair(&f, &x);
    // The founder is offline; member X mints a new link.
    x.daemon(false, "", &[dialog("Allow")]);
    let t = invite(&x, "new");
    c.daemon(false, "", &[peers(&[&x])]);
    let r = join(&c, &t, "carol");
    assert_eq!(r["state"], "admitted", "{r}");
    assert_eq!(r["team"], "acme");
    assert_eq!(members(&x), 3);
    // The founder comes back and converges.
    f.daemon(false, "", &[peers(&[&x])]);
    until(5, "F converges", || {
        members(&f) == 3 && f.heads() == x.heads()
    });
}

/// A bad link is refused by the CLI before any dial.
#[test]
fn bad_link_and_unreachable_texts() {
    let (a, b) = (Env::new(), Env::new());
    a.daemon(false, "", &[dialog("")]);
    let t = invite(&a, "reusable");
    // B knows no address for A: nobody is reachable.
    b.daemon(false, "", &[]);
    let r = join(&b, &t, "bob");
    refused(&r, "unreachable");
    let o = b.detached(&[EXE, "join", &t, "--name", "bob"], "");
    assert!(!o.status.success());
    assert_eq!(
        err(&o),
        "No one from this network is online right now. Ask them to open their laptop, then run the same command again. Your key is ready; nothing was lost.\n"
    );
    refused(&join(&b, "nonsense", "bob"), "bad_ticket");
}
