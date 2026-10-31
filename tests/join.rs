//! Admission: a candidate joins with a ticket; a listed member checks it, asks its person
//! (or admits at once for an open link), writes the `admit` and hands over the roster. Real
//! daemons; a test that needs a hand-built request plays the candidate over its own endpoint.

mod common;

use common::{
    addr, computer, dialog, err, identity, invite, members, out, peers, person, seed_pair, until,
    Env, EXE,
};
use iroh::endpoint::{presets, Connection, RecvStream, SendStream};
use iroh::{Endpoint, SecretKey};
use lightbulb::admission;
use lightbulb::identity::Identity;
use lightbulb::net::frames::{self, Frame};
use lightbulb::net::JOIN_ALPN;
use lightbulb::roster;
use lightbulb::ticket::{self, Ticket};
use serde_json::{json, Value};
use std::time::Duration;

/// An open link's ticket: person-only.
fn invite_open(e: &Env) -> String {
    let r = person(e, json!({"id": 1, "op": "team.invite", "mode": "open"}));
    r["ticket"].as_str().unwrap_or_else(|| panic!("{r}")).into()
}

/// `team.join` (person-only) as `name`.
fn join(e: &Env, ticket: &str, name: &str) -> Value {
    person(
        e,
        json!({"id": 1, "op": "team.join", "ticket": ticket, "name": name}),
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
    _conn: Connection,
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
            _conn: conn,
            _send: send,
            recv,
        }
    }

    /// A candidate of its own key asking to join as `name` with `t`.
    async fn ask(env: &Env, t: &Ticket, name: &str) -> Played {
        let id = identity();
        Played::send(env, &id, &admission::request(&id, t, name, "mbp")).await
    }

    async fn recv(&mut self) -> std::result::Result<Frame, String> {
        tokio::time::timeout(Duration::from_secs(5), frames::read(&mut self.recv))
            .await
            .map_err(|_| "no frame within 5 s".to_string())?
            .map_err(|e| e.to_string())
    }
}

#[test]
fn an_invite_admits_without_a_prompt() {
    // A prompt would decline.
    let (a, b, line) = pair("Decline", "reusable");
    let r = join(&b, &line, "bob");
    assert_eq!(r["state"], "admitted", "{r}");
    assert_eq!(r["team"], "prasiddha");
    converged(&a, &b, 2);
}

#[test]
fn forged_mode_is_refused() {
    let (a, b, t) = pair("Allow", "reusable");
    let mut t = ticket::decode(&t).unwrap();
    t.mode = "ask".into();
    refused(&join(&b, &ticket::encode(&t), "bob"), "bad_ticket");
    assert_eq!((members(&a).len(), members(&b).len()), (1, 1));
}

/// A link's expiry is the inviter's own stamp, compared with the inviter's own clock when the
/// request arrives: the joiner's clock is never read, so a joiner whose clock runs behind
/// cannot use a link the inviter's clock says is over.
#[test]
fn an_expired_link_is_refused_by_the_inviters_clock() {
    let (a, b) = (Env::new(), Env::new());
    let ia = a.identity();
    let now = common::now_s();
    let g = lightbulb::daemon::net_ops::genesis(&ia, "prasiddha", "mac", "acme", now - 120);
    let store = a.store();
    roster::accept(&store, &g, now).unwrap();
    let r = roster::replay(&roster::entries(&store).unwrap());
    let id = format!("{}.{}", ia.short, common::ulid());
    let t = ticket::sign(
        &ia,
        &r,
        id,
        "s3cret".into(),
        "open",
        now - 60,
        (String::new(), vec![]),
        None,
    );
    let link = ticket::link_body(&t, None);
    let entry = roster::Entry::signed(&ia, vec![g.hash()], roster::Kind::Link, link, now - 90);
    roster::accept(&store, &entry, now).unwrap();
    store
        .put_link(&t.ticket_id, &t.secret, None, "open", t.expires_at)
        .unwrap();
    drop(store);
    a.daemon(false, &computer("mac"), &[dialog("Allow")]);
    b.daemon(false, &computer("mbp"), &[peers(&[&a])]);
    refused(&join(&b, &ticket::encode(&t), "carol"), "expired_ticket");
    assert_eq!(members(&a).len(), 1);
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
    until(5, "F converges", || members(&f).len() == 3);
    x.ask(json!({"id": 1, "op": "shutdown"}));
    // The inviter is offline: C2 reaches only F.
    c2.daemon(false, "", &[peers(&[&f])]);
    refused(&join(&c2, &t, "dave"), "bad_ticket");
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
        (String::new(), vec![]),
        None,
    );
    c.daemon(false, "", &[peers(&[&f])]);
    refused(&join(&c, &ticket::encode(&forged), "carol"), "bad_ticket");
    assert_eq!((members(&f).len(), members(&x).len()), (2, 2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn candidate_field_must_be_the_peer_key() {
    let a = Env::new();
    a.daemon(false, "", &[dialog("Allow")]);
    let t = ticket::decode(&invite(&a, "reusable")).unwrap();
    // Signed by `other`, naming `other`, sent over `me`'s endpoint.
    let (me, other) = (identity(), identity());
    let req = admission::request(&other, &t, "carol", "mbp");
    let mut p = Played::send(&a, &me, &req).await;
    let got = p.recv().await;
    assert!(got.is_err(), "answered: {got:?}");
    assert_eq!(members(&a).len(), 1);
}

#[test]
fn link_replaced_on_removal_no_longer_joins() {
    let (a, b, c) = (Env::new(), Env::new(), Env::new());
    seed_pair(&a, &b);
    a.daemon(false, "", &[dialog("Allow")]);
    let t = invite(&a, "reusable");
    let o = a.detached(&[EXE, "remove", "bob"], "");
    assert!(o.status.success(), "{}", err(&o));
    // "send this line instead: lightbulb join '<ticket>' --name NAME" -> the ticket.
    let line = out(&o)
        .split("send this line instead: lightbulb join '")
        .nth(1)
        .unwrap()
        .split('\'')
        .next()
        .unwrap()
        .to_string();
    c.daemon(false, "", &[peers(&[&a])]);
    refused(&join(&c, &t, "carol"), "bad_ticket");
    assert_eq!(members(&a).len(), 1);
    // The link that replaced it does.
    assert_eq!(join(&c, &line, "carol")["state"], "admitted");
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
        "You are in \"acme\". Leave it first: lightbulb leave\n"
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
    let r = join(&c, &t, "carol");
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
    // A is gone (the ticket's addresses answer nothing): nobody is reachable.
    a.ask(json!({"id": 1, "op": "shutdown"}));
    until(10, "A stops", || {
        std::os::unix::net::UnixStream::connect(a.state.path().join("sock")).is_err()
    });
    b.daemon(
        false,
        "",
        &[("LIGHTBULB_CONNECT_DEBUG_DIAL_WAIT_S", "0".into())],
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn candidate_with_a_bad_signature_is_dropped() {
    let a = Env::new();
    a.daemon(false, "", &[dialog("Allow")]);
    let t = ticket::decode(&invite(&a, "reusable")).unwrap();
    // `candidate` is the peer, but the name was changed after signing.
    let me = identity();
    let mut v = serde_json::to_value(admission::request(&me, &t, "carol", "mbp")).unwrap();
    v["name"] = json!("mallory");
    let req: Frame = serde_json::from_value(v).unwrap();
    let mut p = Played::send(&a, &me, &req).await;
    let got = p.recv().await;
    assert!(got.is_err(), "answered: {got:?}");
    assert!(rows(&a).is_empty());
    assert_eq!(members(&a).len(), 1);
}

#[test]
fn admitted_candidate_that_lost_the_roster_rejoins() {
    let (a, b, t) = pair("Allow", "reusable");
    assert_eq!(join(&b, &t, "bob")["state"], "admitted");
    let log = roster::entries(&a.store()).unwrap().len();
    // B loses its roster: back to a solo network.
    b.ask(json!({"id": 1, "op": "shutdown"}));
    until(10, "B stops", || !b.state.path().join("sock").exists());
    b.store().clear_roster().unwrap();
    b.daemon(false, &computer("mbp"), &[peers(&[&a])]);
    assert_eq!(members(&b).len(), 1);
    let r = join(&b, &t, "bob");
    assert_eq!(r["state"], "admitted", "{r}");
    assert_eq!(
        roster::entries(&a.store()).unwrap().len(),
        log,
        "nothing written"
    );
    converged(&a, &b, 2);
}

/// A's link, and a B whose daemon takes this test process for an agent, with `env` set.
fn agent_joiner(env: &[(&str, String)]) -> (Env, Env, String) {
    let (a, b) = (Env::new(), Env::new());
    a.daemon(false, "", &[dialog("Allow")]);
    let t = invite(&a, "reusable");
    let me = std::process::id();
    let birth = lightbulb::os::process(me).unwrap().birth_micros;
    let pin = ("LIGHTBULB_CONNECT_HOOK_CALLER", format!("{me}:{birth}"));
    let mut all = vec![peers(&[&a]), pin];
    all.extend(env.iter().map(|(k, v)| (*k, v.clone())));
    b.daemon(false, "", &all);
    (a, b, t)
}

fn agent_join(b: &Env, t: &str) -> Value {
    b.ask(json!({"id": 1, "op": "team.join", "ticket": t, "name": "bob"}))
}

#[test]
fn an_agent_joins_when_its_person_presses_join() {
    let (a, b, t) = agent_joiner(&[dialog("Join")]);
    let r = agent_join(&b, &t);
    assert_eq!(r["state"], "admitted", "{r}");
    assert_eq!((members(&a).len(), members(&b).len()), (2, 2));
}

#[test]
fn dont_join_refuses_an_agent_and_admits_nothing() {
    let (a, b, t) = agent_joiner(&[dialog("Don't join")]);
    refused(&agent_join(&b, &t), admission::PERSON_SAID_NO);
    assert_eq!((members(&a).len(), members(&b).len()), (1, 1));
    let log = std::fs::read_to_string(a.state.path().join("daemon.log")).unwrap();
    assert!(!log.contains("join: J"), "A heard no request: {log}");
}

/// `dialog("")`: no window can answer (off macOS, headless, SSH), and a timeout reads the same.
/// The CLI runs under this pinned process: the agent is told what its person must do.
#[test]
fn with_no_dialog_an_agent_is_refused_and_told_what_to_do() {
    let (a, b, t) = agent_joiner(&[dialog("")]);
    refused(&agent_join(&b, &t), admission::NEEDS_A_PERSON);
    let o = b
        .base_cmd(std::process::Command::new(EXE))
        .args(["join", &t, "--name", "bob"])
        .output()
        .unwrap();
    assert!(
        err(&o).contains(admission::NEEDS_A_PERSON_TEXT),
        "{}",
        err(&o)
    );
    assert_eq!((members(&a).len(), members(&b).len()), (1, 1));
}

/// The dialog's own words, and Don't join first (the default); a fake `osascript` that gives up.
#[test]
fn the_dialog_says_what_happens_and_giving_up_is_no() {
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("log");
    let fake = common::script(
        d.path(),
        &format!(
            "printf '%s\\n' \"$@\" >{}\necho 'button returned:Join, gave up:true'",
            log.display()
        ),
    );
    let (_a, b, t) = agent_joiner(&[(
        "LIGHTBULB_CONNECT_DEBUG_OSASCRIPT",
        fake.display().to_string(),
    )]);
    refused(&agent_join(&b, &t), admission::NEEDS_A_PERSON);
    assert_eq!(members(&b).len(), 1);
    let shown = std::fs::read_to_string(&log).unwrap();
    let inviter = lightbulb::ticket::decode(&t).unwrap().inviter;
    assert!(
        shown.contains(&format!(
            "An agent on this computer asked to join a team, invited by computer {}. Teammates \
             there will see your sessions' names, folders and status.\nDon't join\nJoin\n",
            &inviter[..12]
        )),
        "{shown}"
    );
}

#[test]
fn a_persons_join_shows_no_dialog() {
    // Any dialog would answer Don't join, and refuse.
    let (a, b) = (Env::new(), Env::new());
    a.daemon(false, "", &[dialog("Allow")]);
    let t = invite(&a, "reusable");
    b.daemon(false, "", &[peers(&[&a]), dialog("Don't join")]);
    let r = join(&b, &t, "bob");
    assert_eq!(r["state"], "admitted", "{r}");
}

/// An unusable ticket bothers no one.
#[test]
fn an_agents_doomed_join_asks_no_one() {
    let (_a, b, _t) = agent_joiner(&[dialog("Don't join")]);
    refused(&agent_join(&b, "nonsense"), admission::BAD_TICKET);
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
    let r = join(&c, &ticket::encode(&t), "carol");
    assert_eq!(r["state"], "admitted", "{r}");
    assert_eq!(r["team"], "acme");
    assert!(rows(&f).is_empty(), "F refused before storing a request");
    assert_eq!(rows(&x), ["admitted"]);
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
        "That name has characters Lightbulb cannot show. Use letters, numbers, spaces and punctuation.\n"
    );
}

/// No PEERS knob, no mDNS, no loopback file (`lan = false`), no relay: the joiner reaches the
/// inviter through the addresses on the ticket alone, and the two stay connected after.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_ticket_addresses_alone_reach_the_inviter() {
    let (a, b) = (Env::new(), Env::new());
    a.daemon(false, &computer("mac"), &[dialog("Allow")]);
    let t = invite(&a, "reusable");
    let addrs = ticket::decode(&t).unwrap().addrs;
    let port = a.status()["port"].to_string();
    assert!(
        !addrs.is_empty() && addrs.iter().all(|s| s.ends_with(&format!(":{port}"))),
        "{addrs:?}"
    );
    b.daemon(false, &computer("mbp"), &[]);
    let r = join(&b, &t, "bob");
    assert_eq!(r["state"], "admitted", "{r}");
    converged(&a, &b, 2);
    until(10, "both online", || {
        a.status()["peers"]["online"] == 1 && b.status()["peers"]["online"] == 1
    });
}

#[test]
fn bad_ticket_addresses_are_refused() {
    use lightbulb::net::parse_addrs;
    let ok = |v: &[&str]| parse_addrs(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert!(ok(&["192.168.0.2:4000", "[fe80::1]:4000"]).is_some());
    assert!(ok(&["169.254.1.1:9"]).is_some(), "link-local stays");
    for bad in [
        &["example.com:4000"][..],
        &["224.0.0.251:5353"],
        &["[ff02::1]:9"],
        &["255.255.255.255:9"],
        &["127.0.0.1:22"],
        &["[::1]:22"],
        &["[::ffff:127.0.0.1]:22"],
        &["[::ffff:224.0.0.251]:5353"],
        &["[::ffff:255.255.255.255]:9"],
        &["0.0.0.0:4000"],
        &["10.0.0.1:0"],
        &["10.0.0.1"],
        &["10.0.0.1:1"; 9],
    ] {
        assert!(ok(bad).is_none(), "{bad:?}");
    }
}

/// A forged ticket (its signature is not its inviter's) is refused before its addresses are
/// kept or dialed.
#[test]
fn a_forged_ticket_leaves_no_address_behind() {
    let (a, b) = (Env::new(), Env::new());
    a.daemon(false, &computer("mac"), &[dialog("")]);
    let mut t = ticket::decode(&invite(&a, "reusable")).unwrap();
    t.addrs = vec!["192.0.2.7:4000".into()];
    b.daemon(false, &computer("mbp"), &[]);
    refused(&join(&b, &ticket::encode(&t), "bob"), "bad_ticket");
    let kept = std::fs::read_to_string(b.state.path().join("addrs.json")).unwrap_or_default();
    assert!(!kept.contains("192.0.2.7"), "{kept}");
}

/// An open link asks no one: `join` never says it is asking.
#[test]
fn an_open_link_join_never_says_asking() {
    let (a, b) = (Env::new(), Env::new());
    a.daemon(false, &computer("mac"), &[]);
    let line = invite_open(&a);
    b.daemon(false, &computer("mbp"), &[peers(&[&a])]);
    let o = b.detached(&[EXE, "join", &line, "--name", "bob"], "");
    assert!(o.status.success(), "{}", err(&o));
    assert!(!out(&o).contains("Asking"), "{}", out(&o));
}

/// `join` (the CLI) with the open link's ticket written as `form` makes: admitted.
fn joins_with(form: fn(&str) -> String) {
    let (a, b, bare) = pair("Decline", "open");
    assert!(ticket::decode(&bare).is_ok());
    let o = b.detached(&[EXE, "join", &form(&bare), "--name", "bob"], "");
    assert!(o.status.success(), "{}{}", out(&o), err(&o));
    converged(&a, &b, 2);
}

#[test]
fn join_takes_the_bare_ticket() {
    joins_with(|t| t.to_string());
}

#[test]
fn join_takes_the_app_link() {
    joins_with(|t| format!("lightbulb://join/{t}"));
}

#[test]
fn invite_new_ends_every_earlier_invite_open_ones_too() {
    let (a, c) = (Env::new(), Env::new());
    a.daemon(false, &computer("mac"), &[dialog("Allow")]);
    let (open, once) = (invite_open(&a), invite(&a, "once"));
    let r = person(&a, json!({"id": 1, "op": "team.invite", "mode": "new"}));
    assert_eq!(r["replaced"], true, "{r}");
    let new = r["ticket"].as_str().unwrap().to_string();
    c.daemon(false, &computer("mbp"), &[peers(&[&a])]);
    refused(&join(&c, &open, "carol"), "bad_ticket");
    refused(&join(&c, &once, "carol"), "bad_ticket");
    assert_eq!(members(&a).len(), 1);
    assert_eq!(join(&c, &new, "carol")["state"], "admitted");
}

#[test]
fn joining_says_nothing_of_asking_and_the_commands_know_no_join_requests() {
    let (a, b, t) = pair("", "reusable");
    let o = b.detached(&[EXE, "join", &t, "--name", "bob"], "");
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(
        out(&o),
        "Connected as bob (mbp). Team \"prasiddha\": 2 computers online.\n\
         Your agents here can now see and message the team's agents. Try: lightbulb who --all\n"
    );
    converged(&a, &b, 2);
    let o = a.detached(&[EXE, "held"], "");
    assert_eq!(out(&o), "Nothing is waiting for your answer.\n");
    let o = a.detached(&[EXE, "approve", "J1"], "");
    assert!(!o.status.success());
    assert_eq!(err(&o), "No prompt J1.\n");
}

/// A fake `osascript` in `dir` that logs every dialog's argv to `log` and answers `Remove` to the
/// notice that has `marker` in its text; any other dialog waits to be withdrawn.
fn notice_osascript(dir: &std::path::Path, marker: &str) -> (&'static str, String) {
    let d = dir.display();
    let p = common::script(
        dir,
        &format!(
            "printf '%s\\n' \"$@\" >>{d}/log\ncase \"$*\" in *'{marker}'*) \
             echo 'button returned:Remove, gave up:false';; *) exec sleep 600;; esac"
        ),
    );
    ("LIGHTBULB_CONNECT_DEBUG_OSASCRIPT", p.display().to_string())
}

/// A pinned (agent) daemon A, with the notice fake; whoever calls `team.invite` from this
/// process is an agent. A's session is registered so the notice names it.
fn agent_daemon(dir: &std::path::Path) -> Env {
    let a = Env::new();
    let me = std::process::id();
    let birth = lightbulb::os::process(me).unwrap().birth_micros;
    let pin = ("LIGHTBULB_CONNECT_HOOK_CALLER", format!("{me}:{birth}"));
    a.daemon(false, "", &[pin, notice_osascript(dir, "invited carol")]);
    a.ask(
        json!({"id": 1, "op": "register", "session": {"provider": "claude",
        "conversation": "c1", "pid": me, "birth_micros": birth, "cwd": "/tmp"}}),
    );
    a
}

async fn carol_joins(a: &Env, ticket: &str) {
    let t = ticket::decode(ticket).unwrap();
    let cand = identity();
    let _p = Played::send(a, &cand, &admission::request(&cand, &t, "carol", "mbp")).await;
    until(10, "carol is in with no Allow", || members(a).len() == 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agents_invite_is_one_time_and_its_use_raises_a_notice_and_remove_removes() {
    let dir = tempfile::tempdir().unwrap();
    let a = agent_daemon(dir.path());
    let r = a.ask(json!({"id": 1, "op": "team.invite", "mode": "reusable"}));
    assert_eq!(r["mode"], "once", "{r}");
    // The fake notice answers Remove at once, so carol may be in and out between two polls:
    // the notice is the proof she was admitted.
    let t = ticket::decode(r["ticket"].as_str().unwrap()).unwrap();
    let cand = identity();
    let _p = Played::send(&a, &cand, &admission::request(&cand, &t, "carol", "mbp")).await;
    let log = || std::fs::read_to_string(dir.path().join("log")).unwrap_or_default();
    // It names the inviting session by its agent, not its handle ("Your claude-1 mailbox").
    until(30, "the notice names the agent and carol", || {
        log().contains("Your Claude Code invited carol into your network.")
    });
    until(30, "carol is removed again", || members(&a).len() == 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_persons_invite_raises_no_notice() {
    let dir = tempfile::tempdir().unwrap();
    let a = agent_daemon(dir.path());
    let t = invite(&a, "reusable");
    carol_joins(&a, &t).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let log = std::fs::read_to_string(dir.path().join("log")).unwrap_or_default();
    assert!(!log.contains("invited"), "{log}");
    assert_eq!(members(&a).len(), 2);
}
