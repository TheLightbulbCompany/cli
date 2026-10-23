//! Admission: a candidate joins with a ticket; a listed member checks it, asks its person
//! (or admits at once for an open link), writes the `admit` and hands over the roster. Real
//! daemons; a test that needs a hand-built request plays the candidate over its own endpoint.

mod common;

use common::{
    addr, computer, dialog, err, identity, invite, members, out, peers, person, seed_pair, until,
    wait_s, Env, EXE,
};
use iroh::endpoint::{presets, Connection, RecvStream, SendStream};
use iroh::{Endpoint, SecretKey};
use lightbulb_connect::admission;
use lightbulb_connect::identity::Identity;
use lightbulb_connect::net::frames::{self, Frame};
use lightbulb_connect::net::JOIN_ALPN;
use lightbulb_connect::roster;
use lightbulb_connect::ticket::{self, Ticket};
use serde_json::{json, Value};
use std::time::Duration;

/// An open link's join line: person-only.
fn invite_open(e: &Env) -> String {
    let r = person(e, json!({"id": 1, "op": "team.invite", "mode": "open"}));
    r["line"].as_str().unwrap_or_else(|| panic!("{r}")).into()
}

/// `team.join` (person-only) as `name`, a member of `kind`.
fn join(e: &Env, ticket: &str, name: &str, kind: &str) -> Value {
    person(
        e,
        json!({"id": 1, "op": "team.join", "ticket": ticket, "name": name, "kind": kind}),
    )
}

/// A solo founder A (computer "mac") whose dialogs answer `button`, its link of `mode`, and a
/// candidate B (computer "mbp") that can reach it.
fn pair(button: &str, mode: &str) -> (Env, Env, String) {
    let (a, b) = (Env::new(), Env::new());
    a.daemon(false, &computer("mac"), &[dialog(button)]);
    let t = if mode == "open" {
        invite_open(&a)
    } else {
        invite(&a, mode)
    };
    b.daemon(false, &computer("mbp"), &[peers(&[&a])]);
    (a, b, t)
}

/// Both list `n` members, with the same heads.
fn converged(a: &Env, b: &Env, n: usize) {
    until(5, "both converge", || {
        members(a).len() == n && members(b).len() == n && a.heads() == b.heads()
    });
}

fn refused(r: &Value, reason: &str) {
    assert_eq!(
        (r["state"].as_str(), r["reason"].as_str()),
        (Some("refused"), Some(reason)),
        "{r}"
    );
}

fn requests(e: &Env) -> Vec<Value> {
    let r = e.ask(json!({"id": 1, "op": "team.join_requests"}));
    r["requests"]
        .as_array()
        .unwrap_or_else(|| panic!("{r}"))
        .clone()
}

fn rows(e: &Env) -> Vec<String> {
    rusqlite::Connection::open(e.state.path().join("state.db"))
        .unwrap()
        .prepare("SELECT state FROM join_requests ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
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
            &admission::request(&id, t, name, "mbp", "computer", None),
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
    let (a, b, t) = pair("", "reusable");
    // `$USER` is A's name too.
    let o = b.detached(&[EXE, "join", &t], "");
    assert!(!o.status.success());
    let taken = format!(
        "The name \"prasiddha\" is already used in this network. Try: lightbulb-connect join '{t}' --name prasiddha-2\n"
    );
    assert_eq!(err(&o), taken);
    std::thread::scope(|s| {
        let joining = s.spawn(|| b.detached(&[EXE, "join", &t, "--name", "bob"], ""));
        until(10, "J1 is listed", || requests(&a).len() == 1);
        let r = &requests(&a)[0];
        assert_eq!(r["id"], "J1");
        // `held` lists it with the command that answers it.
        let o = a.detached(&[EXE, "held"], "");
        assert_eq!(
            out(&o),
            "J1  bob (mbp) wants to join — lightbulb-connect approve J1\n",
            "{}",
            err(&o)
        );
        assert_eq!(r["candidate"], b.identity().key);
        assert_eq!((&r["name"], &r["computer"]), (&json!("bob"), &json!("mbp")));
        assert_eq!((members(&a).len(), members(&b).len()), (1, 1));
        let o = a.detached(&[EXE, "approve", "J1"], "");
        assert!(o.status.success(), "{}", err(&o));
        let o = joining.join().unwrap();
        assert!(o.status.success(), "{}", err(&o));
        assert_eq!(
            out(&o),
            "Asking whoever sent the link to allow this computer… (they get a prompt; this waits up to 10 minutes)\n\
             Connected as bob (mbp). Team \"prasiddha\": 2 computers online.\n\
             Your agents here can now see and message the team's agents. Try: lightbulb-connect who --all\n\
             Until both computers sign in, your other computer counts as another person and its first message needs your Allow.\n"
        );
    });
    converged(&a, &b, 2);
    let o = a.detached(&[EXE, "held"], "");
    assert_eq!(out(&o), "No messages are waiting for your Allow.\n");
    // A second answer finds nothing open.
    let o = a.detached(&[EXE, "decline", "J1"], "");
    assert!(!o.status.success());
    assert_eq!(err(&o), "J1 was already answered or withdrawn.\n");
    let o = a.detached(&[EXE, "approve", "J9"], "");
    assert!(!o.status.success());
    assert_eq!(err(&o), "No join request J9.\n");
}

#[test]
fn decline_refuses_and_writes_nothing() {
    let (a, b, t) = pair("Decline", "reusable");
    let heads = a.heads();
    refused(&join(&b, &t, "bob", "computer"), "declined");
    assert_eq!(a.heads(), heads);
    assert_eq!((members(&a).len(), members(&b).len()), (1, 1));
    assert!(requests(&a).is_empty());
}

#[test]
fn open_ticket_admits_without_prompt() {
    // A prompt would decline.
    let (a, b, line) = pair("Decline", "open");
    let r = join(&b, &line, "bob", "computer");
    assert_eq!(r["state"], "admitted", "{r}");
    assert_eq!(r["team"], "prasiddha");
    converged(&a, &b, 2);
}

#[test]
fn forged_open_mode_is_refused() {
    let (a, b, t) = pair("Allow", "reusable");
    let mut t = ticket::decode(&t).unwrap();
    t.mode = "open".into();
    refused(
        &join(&b, &ticket::encode(&t), "bob", "computer"),
        "bad_ticket",
    );
    assert_eq!((members(&a).len(), members(&b).len()), (1, 1));
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
    assert_eq!(join(&c1, &t, "carol", "computer")["state"], "admitted");
    until(5, "F converges", || members(&f).len() == 3);
    x.ask(json!({"id": 1, "op": "shutdown"}));
    // The inviter is offline: C2 reaches only F.
    c2.daemon(false, "", &[peers(&[&f])]);
    refused(&join(&c2, &t, "dave", "computer"), "bad_ticket");
    assert_eq!(members(&f).len(), 3);
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
        String::new(),
    );
    c.daemon(false, "", &[peers(&[&f])]);
    refused(
        &join(&c, &ticket::encode(&forged), "carol", "computer"),
        "bad_ticket",
    );
    assert_eq!((members(&f).len(), members(&x).len()), (2, 2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn candidate_field_must_be_the_peer_key() {
    let a = Env::new();
    a.daemon(false, "", &[dialog("Allow")]);
    let t = ticket::decode(&invite(&a, "reusable")).unwrap();
    // Signed by `other`, naming `other`, sent over `me`'s endpoint.
    let (me, other) = (identity(), identity());
    let req = admission::request(&other, &t, "carol", "mbp", "computer", None);
    let mut p = Played::send(&a, &me, &req).await;
    let got = p.recv().await;
    assert!(got.is_err(), "answered: {got:?}");
    assert!(requests(&a).is_empty());
    assert_eq!(members(&a).len(), 1);
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
    a.daemon(false, "", &[dialog(""), wait_s(3)]);
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
    assert_eq!(rows(&a), ["expired"; 5]);
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
    refused(&join(&c, &t, "carol", "computer"), "bad_ticket");
    assert_eq!(members(&a).len(), 1);
    // The link that replaced it does.
    assert_eq!(join(&c, &line, "carol", "computer")["state"], "admitted");
}

/// Only an embedded instance joins as a cloud member (`tests/embed.rs`): a computer's person
/// cannot take a workspace's one place.
#[test]
fn a_computer_cannot_join_as_cloud() {
    let (a, b, t) = pair("Allow", "reusable");
    let proof = json!({"kind": "workspace", "sub": "ws:W1", "workspace": {"id": "W1"}});
    let r = person(
        &b,
        json!({"id": 1, "op": "team.join", "ticket": t, "name": "bot", "kind": "cloud",
            "proof": proof.to_string()}),
    );
    assert_eq!(r["error"], "bad_request", "{r}");
    assert_eq!(members(&a).len(), 1);
}

#[test]
fn join_from_a_multi_member_network_is_refused() {
    let (a, b, c) = (Env::new(), Env::new(), Env::new());
    seed_pair(&a, &b);
    b.daemon(false, "", &[]);
    c.daemon(false, "", &[dialog("Allow")]);
    let t = invite(&c, "reusable");
    let heads = b.heads();
    let r = join(&b, &t, "bob2", "computer");
    refused(&r, "already_in_team");
    assert_eq!(r["team"], "acme");
    let o = b.detached(&[EXE, "join", &t], "");
    assert!(!o.status.success());
    assert_eq!(
        err(&o),
        "You are in \"acme\". Leave it first: lightbulb-connect leave\n"
    );
    assert_eq!(b.heads(), heads);
    assert_eq!(members(&c).len(), 1);
}

#[test]
fn non_admin_inviter_admits_its_own_invitee_with_no_admin_online() {
    let (f, x, c) = (Env::new(), Env::new(), Env::new());
    seed_pair(&f, &x);
    // The founder is offline; member X mints a new link.
    x.daemon(false, "", &[dialog("Allow")]);
    let t = invite(&x, "new");
    c.daemon(false, "", &[peers(&[&x])]);
    let r = join(&c, &t, "carol", "computer");
    assert_eq!(r["state"], "admitted", "{r}");
    assert_eq!(r["team"], "acme");
    assert_eq!(members(&x).len(), 3);
    // The founder comes back and converges.
    f.daemon(false, "", &[peers(&[&x])]);
    until(5, "F converges", || {
        members(&f).len() == 3 && f.heads() == x.heads()
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
    let r = join(&b, &t, "bob", "computer");
    refused(&r, "unreachable");
    let o = b.detached(&[EXE, "join", &t, "--name", "bob"], "");
    assert!(!o.status.success());
    assert_eq!(
        err(&o),
        "No one from this network is online right now. Ask them to open their laptop, then run the same command again. Your key is ready; nothing was lost.\n"
    );
    refused(&join(&b, "nonsense", "bob", "computer"), "bad_ticket");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn candidate_with_a_bad_signature_is_dropped() {
    let a = Env::new();
    a.daemon(false, "", &[dialog("Allow")]);
    let t = ticket::decode(&invite(&a, "reusable")).unwrap();
    // `candidate` is the peer, but the name was changed after signing.
    let me = identity();
    let mut v = serde_json::to_value(admission::request(
        &me, &t, "carol", "mbp", "computer", None,
    ))
    .unwrap();
    v["name"] = json!("mallory");
    let req: Frame = serde_json::from_value(v).unwrap();
    let mut p = Played::send(&a, &me, &req).await;
    let got = p.recv().await;
    assert!(got.is_err(), "answered: {got:?}");
    assert!(rows(&a).is_empty());
    assert_eq!(members(&a).len(), 1);
}

#[test]
fn unanswered_request_is_waiting_and_asks_no_one_else() {
    let (f, x, c) = (Env::new(), Env::new(), Env::new());
    seed_pair(&f, &x);
    x.daemon(false, "", &[dialog(""), wait_s(2)]);
    f.daemon(false, "", &[peers(&[&x]), dialog(""), wait_s(2)]);
    let t = invite(&x, "reusable");
    c.daemon(false, "", &[peers(&[&x, &f]), wait_s(2)]);
    let r = join(&c, &t, "carol", "computer");
    assert_eq!(r["state"], "waiting", "{r}");
    assert_eq!(rows(&x), ["expired"]);
    assert!(rows(&f).is_empty(), "the second member was asked");
    assert_eq!(members(&c).len(), 1);
}

#[test]
fn admitted_candidate_that_lost_the_roster_rejoins() {
    let (a, b, t) = pair("Allow", "reusable");
    assert_eq!(join(&b, &t, "bob", "computer")["state"], "admitted");
    let log = roster::entries(&a.store()).unwrap().len();
    // B loses its roster: back to a solo network.
    b.ask(json!({"id": 1, "op": "shutdown"}));
    until(10, "B stops", || !b.state.path().join("sock").exists());
    b.store().clear_roster().unwrap();
    b.daemon(false, &computer("mbp"), &[peers(&[&a])]);
    assert_eq!(members(&b).len(), 1);
    let r = join(&b, &t, "bob", "computer");
    assert_eq!(r["state"], "admitted", "{r}");
    assert_eq!(
        roster::entries(&a.store()).unwrap().len(),
        log,
        "nothing written"
    );
    converged(&a, &b, 2);
}

#[test]
fn team_join_is_person_only() {
    let (a, b) = (Env::new(), Env::new());
    a.daemon(false, "", &[dialog("Allow")]);
    let t = invite(&a, "reusable");
    // B's daemon takes this test process for an agent.
    let me = std::process::id();
    let birth = lightbulb_connect::os::process(me).unwrap().birth_micros;
    let pin = ("LIGHTBULB_CONNECT_HOOK_CALLER", format!("{me}:{birth}"));
    b.daemon(false, "", &[peers(&[&a]), pin]);
    let r = b.ask(json!({"id": 1, "op": "team.join", "ticket": t, "name": "bob"}));
    assert_eq!(
        (r["error"].as_str(), r["reason"].as_str()),
        (Some("bad_request"), Some("not_a_person")),
        "{r}"
    );
    assert_eq!((members(&a).len(), members(&b).len()), (1, 1));
}

#[test]
fn bad_ticket_at_the_first_member_tries_the_next() {
    let (f, x, c) = (Env::new(), Env::new(), Env::new());
    seed_pair(&f, &x);
    // F and X are apart: F never receives X's new link.
    x.daemon(false, "", &[dialog("Allow")]);
    f.daemon(false, "", &[dialog("Allow")]);
    let mut t = ticket::decode(&invite(&x, "new")).unwrap();
    // X lists F first.
    t.members = vec![f.identity().key, x.identity().key];
    t.sig = x.identity().sign(&serde_json::to_value(&t).unwrap());
    c.daemon(false, "", &[peers(&[&f, &x])]);
    let r = join(&c, &ticket::encode(&t), "carol", "computer");
    assert_eq!(r["state"], "admitted", "{r}");
    assert_eq!(r["team"], "acme");
    assert!(rows(&f).is_empty(), "F refused before storing a request");
    assert_eq!(rows(&x), ["admitted"]);
}

/// A computer proof valid `secs` more seconds, for `id`'s key.
fn short_proof(id: &Identity, secs: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    common::issuer::mint(&json!({"iss": "test", "iat": now, "exp": now + secs,
        "sub": "person-c", "name": "Carol", "computer": id.key, "kind": "computer",
        "workspace": null, "agents": []}))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_proof_that_expires_while_its_dialog_waits_is_refused_at_allow() {
    let a = Env::new();
    a.daemon(false, "", &[dialog("")]);
    let t = ticket::decode(&invite(&a, "reusable")).unwrap();
    let me = identity();
    let token = short_proof(&me, 2);
    let req = admission::request(&me, &t, "carol", "mbp", "computer", Some(&token));
    let _p = Played::send(&a, &me, &req).await;
    until(10, "J1 is listed", || requests(&a).len() == 1);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let r = person(
        &a,
        json!({"id": 1, "op": "team.admit", "request": "J1", "verdict": "allow"}),
    );
    assert_eq!(r["state"], "refused", "{r}");
    assert_eq!(members(&a).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unprintable_name_is_refused_as_bad_name() {
    let (a, b) = (Env::new(), Env::new());
    a.daemon(false, "", &[dialog("Allow")]);
    let line = invite(&a, "reusable");
    let t = ticket::decode(&line).unwrap();
    let mut p = Played::ask(&a, &t, "car\u{202e}ol").await;
    match p.recv().await.unwrap() {
        Frame::JoinDecision { reason, .. } => assert_eq!(reason.as_deref(), Some("bad_name")),
        f => panic!("{f:?}"),
    }
    // The candidate's own CLI says it in a sentence, before any dial.
    b.daemon(false, "", &[peers(&[&a])]);
    let o = b.detached(&[EXE, "join", &line, "--name", "car\u{202e}ol"], "");
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        err(&o),
        "That name has characters Lightbulb Connect cannot show. Use letters, numbers, spaces and punctuation.\n"
    );
}
