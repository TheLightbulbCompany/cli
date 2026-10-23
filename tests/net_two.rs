//! Two computers talking: the QUIC transport, the membership gate and delta roster sync.
//! Each test runs real daemons; a test that needs to see (or hand-build) frames plays one
//! member itself, with that member's key, over its own iroh endpoint.

use iroh::Endpoint;
use lightbulb::identity::Identity;
use lightbulb::net::frames::{self, Frame};
use lightbulb::net::{ALPN, PROTOCOL};
use lightbulb::roster::{self, Entry, Kind};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Duration;

mod common;

use common::{addr, endpoint, seed_pair, until, Env, Peer, EXE};

const PAGE_BYTES: usize = 512 * 1024;

impl Peer {
    /// The `roster` frames up to and including the one with `more: false`.
    async fn pages(&mut self) -> Vec<Frame> {
        let mut out = vec![];
        loop {
            let f = self.recv().await;
            let Frame::Roster { more, .. } = &f else {
                panic!("expected roster, got {f:?}");
            };
            let last = !more;
            out.push(f);
            if last {
                return out;
            }
        }
    }
}

fn entries_of(pages: &[Frame]) -> Vec<Entry> {
    pages
        .iter()
        .flat_map(|f| match f {
            Frame::Roster { entries, .. } => entries.clone(),
            _ => vec![],
        })
        .collect()
}

fn heads(v: Value) -> Vec<String> {
    serde_json::from_value(v).unwrap()
}

/// `n` `meta` entries by `by`, each the child of the one before, the first a child of `parent`.
fn chain(by: &Identity, parent: &Entry, n: usize, pad: usize, tag: &str) -> Vec<Entry> {
    let mut out: Vec<Entry> = vec![];
    for i in 0..n {
        let p = out.last().unwrap_or(parent).hash();
        let body = json!({"note": format!("{tag}.{i}"), "pad": "x".repeat(pad)});
        out.push(Entry::signed(by, vec![p], Kind::Meta, body, 3));
    }
    out
}

fn store_all(env: &Env, entries: &[Entry]) {
    let store = env.store();
    for e in entries {
        assert!(roster::accept(&store, e, 10).unwrap());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_members_connect_and_converge_rosters() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    b.daemon(false, "", &[]);
    a.daemon(false, "", &[("LIGHTBULB_CONNECT_PEERS", b.peer_arg())]);
    let connected = json!({"online": 1, "direct": 1, "total": 1});
    until(5, "both online", || {
        a.status()["peers"] == connected && b.status()["peers"] == connected
    });
    assert_eq!(a.status()["relay"], "none");
    // A writes (a link entry), B follows.
    let before = a.heads();
    let r = a.ask(json!({"id": 1, "op": "team.invite"}));
    assert_eq!(r["ok"], true, "{r}");
    let after = a.heads();
    assert_ne!(after, before);
    until(2, "B's heads equal A's", || b.heads() == after);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sync_sends_only_the_delta_in_pages() {
    let (a, b) = (Env::new(), Env::new());
    let [g, admit] = seed_pair(&a, &b);
    // B lags A by 1 200 entries: 120 branches of 10.
    let ia = a.identity();
    let lag: Vec<Entry> = (0..120)
        .flat_map(|c| chain(&ia, &admit, 10, 0, &c.to_string()))
        .collect();
    store_all(&a, &lag);
    a.daemon(false, "", &[]);

    let ep = endpoint(&b.identity()).await;
    let mut p = Peer::dial(&ep, &a).await;
    p.hello(vec![admit.hash()]).await;
    let Frame::Hello {
        protocol,
        heads: a_heads,
        ..
    } = p.recv().await
    else {
        panic!("hello first");
    };
    assert_eq!((protocol, a_heads.len()), (PROTOCOL, 120));
    let pages = p.pages().await;
    let more: Vec<bool> = pages
        .iter()
        .map(|f| matches!(f, Frame::Roster { more: true, .. }))
        .collect();
    assert_eq!(more, [true, true, false]);
    let sent = entries_of(&pages);
    assert_eq!(sent.len(), 1_200);
    // Parents before children, across page boundaries.
    let mut seen: HashSet<String> = [g.hash(), admit.hash()].into();
    for e in &sent {
        assert!(
            e.parents.iter().all(|p| seen.contains(p)),
            "parent after child"
        );
        assert!(seen.insert(e.hash()));
    }
    // An up-to-date peer gets nothing.
    p.hello(a_heads).await;
    assert!(entries_of(&p.pages().await).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_before_parent_in_one_frame_still_converges() {
    let (a, b) = (Env::new(), Env::new());
    let [_, admit] = seed_pair(&a, &b);
    let pc = chain(&a.identity(), &admit, 2, 0, "pc");
    b.daemon(false, "", &[]);

    let ep = endpoint(&a.identity()).await;
    let mut p = synced(&ep, &b, &admit).await;
    p.send(&roster_frame(vec![pc[1].clone(), pc[0].clone()]))
        .await;
    until(2, "B applies both", || {
        heads(b.heads()) == vec![pc[1].hash()]
    });
    assert_eq!(roster::entries(&b.store()).unwrap().len(), 4);
}

/// Dials `env`'s daemon as `id` and finishes the opening exchange (`hello`s, empty delta).
async fn synced(ep: &Endpoint, env: &Env, head: &Entry) -> Peer {
    let mut p = Peer::dial(ep, env).await;
    p.hello(vec![head.hash()]).await;
    assert!(matches!(p.recv().await, Frame::Hello { .. }));
    assert!(entries_of(&p.pages().await).is_empty());
    p
}

fn roster_frame(entries: Vec<Entry>) -> Frame {
    Frame::Roster {
        entries,
        more: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_and_parent_in_separate_frames_converge() {
    let (a, b) = (Env::new(), Env::new());
    let [_, admit] = seed_pair(&a, &b);
    let pc = chain(&a.identity(), &admit, 2, 0, "pc");
    b.daemon(false, "", &[]);

    let ep = endpoint(&a.identity()).await;
    let mut p = synced(&ep, &b, &admit).await;
    p.send(&roster_frame(vec![pc[1].clone()])).await;
    p.send(&roster_frame(vec![pc[0].clone()])).await;
    until(2, "B applies both", || {
        heads(b.heads()) == vec![pc[1].hash()]
    });
    assert_eq!(roster::entries(&b.store()).unwrap().len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waiting_entries_are_dropped_on_close() {
    let (a, b) = (Env::new(), Env::new());
    let [_, admit] = seed_pair(&a, &b);
    let pc = chain(&a.identity(), &admit, 2, 0, "pc");
    b.daemon(false, "", &[]);

    let ep = endpoint(&a.identity()).await;
    let mut p = synced(&ep, &b, &admit).await;
    p.send(&roster_frame(vec![pc[1].clone()])).await;
    // B answers this hello only after it has read the child: the child is waiting.
    p.hello(vec![admit.hash()]).await;
    assert!(entries_of(&p.pages().await).is_empty());
    p.conn.close(0u32.into(), b"");

    // The parent on a new connection: only the parent lands.
    let mut q = synced(&ep, &b, &admit).await;
    q.send(&roster_frame(vec![pc[0].clone()])).await;
    until(2, "B applies the parent", || {
        roster::entries(&b.store()).unwrap().len() >= 3
    });
    assert_eq!(heads(b.heads()), vec![pc[0].hash()]);
    assert_eq!(roster::entries(&b.store()).unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn roster_pages_split_by_bytes_not_only_count() {
    let (a, b) = (Env::new(), Env::new());
    let [_, admit] = seed_pair(&a, &b);
    store_all(&a, &chain(&a.identity(), &admit, 300, 4096, "big"));
    a.daemon(false, "", &[]);
    b.daemon(false, "", &[]);

    // A's pages, as B would get them.
    let ep_b = endpoint(&b.identity()).await;
    let mut from_a = Peer::dial(&ep_b, &a).await;
    from_a.hello(vec![admit.hash()]).await;
    assert!(matches!(from_a.recv().await, Frame::Hello { .. }));
    let pages = from_a.pages().await;
    assert!(pages.len() > 1, "{} frame(s)", pages.len());
    for f in &pages {
        let size = serde_json::to_vec(f).unwrap().len();
        assert!(size <= PAGE_BYTES, "a {size}-byte frame");
    }
    assert_eq!(entries_of(&pages).len(), 300);

    // Handed to B's daemon as A: all applied.
    let ep_a = endpoint(&a.identity()).await;
    let mut to_b = synced(&ep_a, &b, &admit).await;
    for f in &pages {
        to_b.send(f).await;
    }
    until(5, "B applies every page", || b.heads() == a.heads());
    assert_eq!(roster::entries(&b.store()).unwrap().len(), 302);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_member_is_refused_on_member_alpn() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    a.daemon(false, "", &[]);
    let stranger = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
    let ep = endpoint(&stranger).await;
    let conn = ep.connect(addr(&a), ALPN).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), conn.closed())
        .await
        .expect("closed within 2 s");
    assert_eq!(
        a.status()["peers"],
        json!({"online": 0, "direct": 0, "total": 1})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removed_member_connection_is_closed_on_removal() {
    let (a, b) = (Env::new(), Env::new());
    let [_, admit] = seed_pair(&a, &b);
    a.daemon(false, "", &[]);
    let ep = endpoint(&b.identity()).await;
    let p = synced(&ep, &a, &admit).await;
    until(2, "A sees B online", || a.status()["peers"]["online"] == 1);

    let o = a.detached(&[EXE, "remove", "bob"], "");
    assert!(o.status.success(), "{}", common::err(&o));
    // Told first (the `remove`, nothing after it), then closed even though it never closes.
    let mut p = p;
    let told = entries_of(&p.pages().await);
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0].kind, Kind::Remove);
    tokio::time::timeout(Duration::from_secs(6), p.conn.closed())
        .await
        .expect("closed within 6 s of the removal");
    // A redial is refused.
    if let Ok(conn) = ep.connect(addr(&a), ALPN).await {
        tokio::time::timeout(Duration::from_secs(2), conn.closed())
            .await
            .expect("redial closed");
    }
    assert_eq!(
        a.status()["peers"],
        json!({"online": 0, "direct": 0, "total": 0})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removal_inside_the_gate_closes_the_connection() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    let pause = ("LIGHTBULB_CONNECT_DEBUG_GATE_PAUSE_MS", "1500".to_string());
    a.daemon(false, "", &[pause]);
    let ep = endpoint(&b.identity()).await;
    let conn = ep.connect(addr(&a), ALPN).await.unwrap();
    // A holds its gate 1.5 s after the handshake; the removal lands in that window and
    // waits for the adoption, so its install closes the adopted connection.
    let o = a.detached(&[EXE, "remove", "bob"], "");
    assert!(o.status.success(), "{}", common::err(&o));
    tokio::time::timeout(Duration::from_secs(3), conn.closed())
        .await
        .expect("closed within 3 s of the removal");
    assert_eq!(
        a.status()["peers"],
        json!({"online": 0, "direct": 0, "total": 0})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn older_protocol_is_reported_not_served() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    a.daemon(false, "", &[]);
    // B, played here, speaks protocol 0.
    let ep = endpoint(&b.identity()).await;
    let mut p = Peer::dial(&ep, &a).await;
    p.send(&Frame::Hello {
        protocol: 0,
        heads: vec![],
        addrs: vec![],
    })
    .await;
    assert!(matches!(p.recv().await, Frame::Hello { .. }));
    let notice = json!(["mbp needs an update (protocol 0, yours 1)"]);
    until(5, "A reports B", || a.status()["needs_update"] == notice);
    assert_eq!(a.status()["peers"]["online"], 0);
    // A writes: nothing follows its hello, neither the delta nor the push.
    assert_eq!(a.ask(json!({"id": 1, "op": "team.invite"}))["ok"], true);
    let next = tokio::time::timeout(Duration::from_secs(2), frames::read(&mut p.recv)).await;
    assert!(next.is_err(), "a frame after hello: {next:?}");
}

/// No `LIGHTBULB_CONNECT_PEERS` and no relay, `lan = true` (the default): two daemons on
/// one computer find each other as two release builds do. This test cannot use mDNS: macOS
/// refuses the multicast of an unsigned test binary (`EHOSTUNREACH`, Local Network), so it
/// proves the loopback path (`net::local`, the same `Peers::addr` every dial and join uses),
/// and that the daemon reports a refused mDNS instead of hiding it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_members_on_one_computer_find_each_other() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    b.daemon(true, "", &[]);
    a.daemon(true, "", &[]);
    let connected = json!({"online": 1, "direct": 1, "total": 1});
    until(15, "both online with no PEERS", || {
        a.status()["peers"] == connected && b.status()["peers"] == connected
    });
    let lan = a.status()["lan"].as_str().unwrap().to_string();
    assert!(
        ["on", "blocked", "no network"].contains(&lan.as_str()),
        "{lan}"
    );
    let log = std::fs::read_to_string(a.state.path().join("daemon.log")).unwrap();
    assert_eq!(lan == "blocked", log.contains("Local Network"), "{log}");
}

/// A removed member is told: its `team` says so, it is on its own network again, and mail it
/// still had queued for the team is refused (`not_member`) at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removed_member_is_told() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    // B never hears an ack, so its mail to A stays queued.
    let drop_acks = ("LIGHTBULB_CONNECT_DEBUG_DROP_ACKS", "1".to_string());
    b.daemon(true, "", &[drop_acks]);
    a.daemon(true, "", &[]);
    until(15, "both online", || {
        a.status()["peers"]["online"] == 1 && b.status()["peers"]["online"] == 1
    });
    let (sa, sb) = (common::session(&a, "a1"), common::session(&b, "b1"));
    b.send(&sb, &sa, "queued at removal", "r1", None);
    assert_eq!(b.state(&sb, "r1"), "queued");

    let o = a.detached(&[EXE, "remove", "bob"], "");
    assert!(o.status.success(), "{}", common::err(&o));
    until(10, "B is on its own network", || {
        b.ask(json!({"id": 1, "op": "team.get"}))["members"]
            .as_array()
            .is_some_and(|m| m.len() == 1)
    });
    let r = b.receipt(&sb, "r1");
    assert_eq!(
        (&r["state"], &r["reason"]),
        (&json!("refused"), &json!("not_member")),
        "{r}"
    );
    let o = b.detached(&[EXE, "team"], "");
    let text = common::out(&o);
    let today = &lightbulb::date::utc(common::now_s())[..10];
    let line = format!(
        "You were removed from \"acme\" by {} on {today}. This computer is on its own network again.",
        a.ask(json!({"id": 1, "op": "team.get"}))["members"][0]["name"].as_str().unwrap()
    );
    assert!(text.starts_with(&line), "{text}");
    let log = std::fs::read_to_string(b.state.path().join("daemon.log")).unwrap();
    assert_eq!(
        log.matches("net: removed from \"acme\"").count(),
        1,
        "{log}"
    );
    // Alone now, there is nothing to leave.
    let o = b.detached(&[EXE, "leave"], "y\n");
    assert!(o.status.success(), "{}", common::err(&o));
    assert_eq!(
        common::out(&o),
        "This computer is not in a team with anyone; it is on its own network already.\n"
    );
}

/// A removed member's addresses leave addrs.json at the removal, and stay gone after a
/// restart; the file is private.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_members_addresses_are_forgotten() {
    use std::os::unix::fs::MetadataExt;
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    common::start_pair(&a, &b, "", &[]);
    let file = a.state.path().join("addrs.json");
    let bob = b.identity().key;
    until(10, "A keeps B's addresses", || {
        std::fs::read_to_string(&file).is_ok_and(|t| t.contains(&bob))
    });
    assert_eq!(std::fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
    let o = a.detached(&[EXE, "remove", "bob"], "");
    assert!(o.status.success(), "{}", common::err(&o));
    let kept = std::fs::read_to_string(&file).unwrap();
    assert!(!kept.contains(&bob), "{kept}");
    a.ask(json!({"id": 1, "op": "shutdown"}));
    until(10, "A stops", || {
        std::os::unix::net::UnixStream::connect(a.state.path().join("sock")).is_err()
    });
    a.daemon(false, "", &[]);
    let kept = std::fs::read_to_string(&file).unwrap();
    assert!(!kept.contains(&bob), "{kept}");
}

/// The loopback file is private, names its daemon's pid, and goes when SIGTERM stops it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loopback_file_is_withdrawn_on_sigterm() {
    use std::os::unix::fs::MetadataExt;
    let a = Env::new();
    a.daemon(true, "", &[]);
    let uid = unsafe { libc::getuid() };
    let file = std::path::PathBuf::from(format!("/tmp/lightbulb-{uid}")).join(a.identity().key);
    let pid: i32 = std::fs::read_to_string(a.state.path().join("daemon.lock"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let text = std::fs::read_to_string(&file).unwrap();
    assert_eq!(text, format!("127.0.0.1:{} {pid}", a.status()["port"]));
    assert_eq!(std::fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    until(10, "the file is withdrawn", || !file.exists());
    until(
        10,
        "the daemon exits",
        || unsafe { libc::kill(pid, 0) } != 0,
    );
}

/// A removed member that dials in again is told only its removal: never a link minted (with
/// its secret) or a member admitted after it, whatever heads it claims.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removed_member_hears_its_removal_and_nothing_after() {
    let (a, b) = (Env::new(), Env::new());
    let [genesis, _] = seed_pair(&a, &b);
    a.daemon(false, "", &[]);
    let o = a.detached(&[EXE, "remove", "bob"], "");
    assert!(o.status.success(), "{}", common::err(&o));
    let later = common::invite(&a, "reusable");
    let later = lightbulb::ticket::decode(&later).unwrap().ticket_id;
    // And a member admitted after the removal: carol, through an open link (no dialog).
    let open = common::person(&a, json!({"id": 1, "op": "team.invite", "mode": "open"}));
    let c = Env::new();
    c.daemon(false, "", &[common::peers(&[&a])]);
    let r = common::person(
        &c,
        json!({"id": 1, "op": "team.join", "ticket": open["line"], "name": "carol", "kind": "computer"}),
    );
    assert_eq!(r["state"], "admitted", "{r}");
    let carol = c.identity().key;
    let ep = endpoint(&b.identity()).await;
    // Claiming to know only the genesis: everything after it would be the whole log.
    for heads in [vec![genesis.hash()], vec![]] {
        let mut p = Peer::dial(&ep, &a).await;
        p.hello(heads).await;
        let told = entries_of(&p.pages().await);
        assert!(told.iter().any(|e| e.kind == Kind::Remove), "{told:?}");
        assert!(
            !told
                .iter()
                .any(|e| e.kind == Kind::Link && e.body["ticket_id"] == later.as_str()),
            "a later link leaked: {told:?}"
        );
        assert!(
            !told.iter().any(|e| e.body.to_string().contains(&carol)),
            "a later member leaked: {told:?}"
        );
        p.conn.close(0u32.into(), b"");
    }
}

/// The loopback file is written again each round of the dial loop (here forced by a roster
/// change), so macOS's cleanup of untouched /tmp files does not lose a long-running daemon:
/// deleted, it comes back with its text.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loopback_file_is_kept_fresh() {
    let a = Env::new();
    a.daemon(true, "", &[]);
    let uid = unsafe { libc::getuid() };
    let file = std::path::PathBuf::from(format!("/tmp/lightbulb-{uid}")).join(a.identity().key);
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::remove_file(&file).unwrap();
    common::invite(&a, "reusable");
    until(5, "the file is back", || {
        std::fs::read_to_string(&file).is_ok_and(|t| t == text)
    });
}
