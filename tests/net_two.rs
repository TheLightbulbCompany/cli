//! Two computers talking: the QUIC transport, the membership gate and delta roster sync.
//! Each test runs real daemons; a test that needs to see (or hand-build) frames plays one
//! member itself, with that member's key, over its own iroh endpoint.

use iroh::endpoint::{presets, Connection, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, PublicKey, SecretKey};
use lightbulb_network::identity::{self, Identity};
use lightbulb_network::net::frames::{self, Frame};
use lightbulb_network::net::{ALPN, PROTOCOL};
use lightbulb_network::roster::{self, Entry, Kind};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Duration;

mod common;

use common::{seed_pair, until, Env, EXE};

const PAGE_BYTES: usize = 512 * 1024;

/// A member played by the test: its own endpoint, holding `id`'s key.
async fn endpoint(id: &Identity) -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .secret_key(SecretKey::from_bytes(&id.signing.to_bytes()))
        .bind()
        .await
        .unwrap()
}

/// The running daemon of `env`, on the loopback.
fn addr(env: &Env) -> EndpointAddr {
    let key = PublicKey::from_bytes(&identity::key_bytes(&env.identity().key).unwrap()).unwrap();
    let port = env.status()["port"].as_u64().unwrap();
    EndpointAddr::new(key).with_ip_addr(format!("127.0.0.1:{port}").parse().unwrap())
}

struct Peer {
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
}

impl Peer {
    /// Dials `env`'s daemon on the member ALPN and opens the connection's one stream.
    async fn dial(ep: &Endpoint, env: &Env) -> Peer {
        let conn = ep.connect(addr(env), ALPN).await.unwrap();
        let (send, recv) = conn.open_bi().await.unwrap();
        Peer { conn, send, recv }
    }

    async fn send(&mut self, f: &Frame) {
        frames::write(&mut self.send, f).await.unwrap();
    }

    async fn hello(&mut self, heads: Vec<String>) {
        let protocol = PROTOCOL;
        self.send(&Frame::Hello { protocol, heads }).await;
    }

    async fn recv(&mut self) -> Frame {
        tokio::time::timeout(Duration::from_secs(5), frames::read(&mut self.recv))
            .await
            .expect("a frame within 5 s")
            .unwrap()
    }

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
    a.daemon(false, "", &[("LIGHTBULB_NETWORK_PEERS", b.peer_arg())]);
    let connected = json!({"online": 1, "total": 1});
    until(5, "both online", || {
        a.status()["peers"] == connected && b.status()["peers"] == connected
    });
    assert_eq!(a.status()["relay"], "off");
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
    assert_eq!(a.status()["peers"], json!({"online": 0, "total": 1}));
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
    tokio::time::timeout(Duration::from_secs(2), p.conn.closed())
        .await
        .expect("closed within 2 s of the removal");
    // A redial is refused.
    if let Ok(conn) = ep.connect(addr(&a), ALPN).await {
        tokio::time::timeout(Duration::from_secs(2), conn.closed())
            .await
            .expect("redial closed");
    }
    assert_eq!(a.status()["peers"], json!({"online": 0, "total": 0}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removal_inside_the_gate_closes_the_connection() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    let pause = ("LIGHTBULB_NETWORK_DEBUG_GATE_PAUSE_MS", "1500".to_string());
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
    assert_eq!(a.status()["peers"], json!({"online": 0, "total": 0}));
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
