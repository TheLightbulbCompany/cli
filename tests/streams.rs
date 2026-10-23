//! Streams: a viewer watching a session's live bytes over its own QUIC stream, on a signed
//! `share_grant` (or as the same person's computer). Real daemons; the forged grants come from
//! a member the test plays (`Peer`).

use iroh::endpoint::{RecvStream, SendStream};
use iroh::Endpoint;
use lightbulb_connect::admission::signed;
use lightbulb_connect::daemon::net_ops;
use lightbulb_connect::identity::Identity;
use lightbulb_connect::net::frames::{self, Frame};
use lightbulb_connect::roster::{self, Entry};
use lightbulb_connect::streams;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

mod common;

use common::{admit, endpoint, identity, now_s as now, pair, peers, person, session};
use common::{start_pair, ulid, until, Env, Peer};

/// The session every test streams: `c1` on `e`.
fn c1(e: &Env) -> String {
    session(e, "c1")
}

fn b64(s: &[u8]) -> String {
    data_encoding::BASE64.encode(s)
}

/// A `subscribe`d connection: its events, read on a thread.
struct Sub(mpsc::Receiver<Value>);

impl Sub {
    fn new(e: &Env) -> Sub {
        let mut s = UnixStream::connect(e.state.path().join("sock")).unwrap();
        writeln!(s, r#"{{"id":1,"op":"subscribe"}}"#).unwrap();
        // Events reach a connection only once its `subscribe` is handled: wait for the ack.
        let mut lines = BufReader::new(s).lines();
        let ack: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        assert_eq!(ack["ok"], true, "{ack}");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in lines {
                let Ok(v) = serde_json::from_str::<Value>(&line.unwrap_or_default()) else {
                    return;
                };
                if v.get("event").is_some() && tx.send(v).is_err() {
                    return;
                }
            }
        });
        Sub(rx)
    }

    /// The next `event` within 10 s (others skipped).
    fn next(&self, event: &str) -> Value {
        loop {
            let v = self
                .0
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("no {event} event"));
            if v["event"] == event {
                return v;
            }
        }
    }

    /// Whether an `event` arrives within `ms`.
    fn any(&self, event: &str, ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(ms);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.0.recv_timeout(left) {
                Ok(v) if v["event"] == event => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
    }
}

fn grant(owner: &Env, session: &str, viewer: &str, input: bool, expires_at: i64) -> Value {
    person(
        owner,
        json!({"id": 1, "op": "share.grant", "session": session, "viewer": viewer,
            "input": input, "expires_at": expires_at}),
    )
}

/// `stream.open` from `viewer`, retried until the grant has arrived: its `stream_id`.
fn open(viewer: &Env, session: &str) -> String {
    let mut r = Value::Null;
    until(10, "stream.open succeeds", || {
        r = viewer.ask(json!({"id": 1, "op": "stream.open", "session": session}));
        r["ok"] == true
    });
    r["stream_id"].as_str().unwrap().into()
}

fn send(e: &Env, stream_id: &str, data: &[u8]) -> Value {
    e.ask(json!({"id": 1, "op": "stream.send", "stream_id": stream_id, "data_b64": b64(data)}))
}

fn dropped(e: &Env, what: &str) -> u64 {
    // A kind nothing of was dropped is absent.
    e.status()["dropped"][what].as_u64().unwrap_or(0)
}

fn seed(envs: &[&Env], entries: &[Entry]) {
    for e in envs {
        let store = e.store();
        for x in entries {
            assert!(roster::accept(&store, x, 10).unwrap());
        }
    }
}

/// A and B, two computers of the same person (B's admit carries A's `person_id`).
fn same_person() -> (Env, Env) {
    let (a, b) = (Env::new(), Env::new());
    let (ia, ib) = (a.identity(), b.identity());
    let g = net_ops::genesis(&ia, "prasiddha", "mac", "acme", 1);
    let ab = admit(
        &ia,
        &g,
        &ib,
        "mbp",
        json!({"person_id": ia.key, "proof": "t"}),
    );
    seed(&[&a, &b], &[g, ab]);
    start_pair(&a, &b, "", &[]);
    (a, b)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_person_opens_without_grant() {
    let (a, b) = same_person();
    let (sa, sb) = (Sub::new(&a), Sub::new(&b));
    let viewer_id = open(&b, &c1(&a));
    let opened = sa.next("stream_opened");
    assert_eq!(opened["viewer"], b.identity().key);
    let owner_id = opened["stream_id"].as_str().unwrap();
    assert_eq!(send(&a, owner_id, b"hello")["ok"], true);
    let got = sb.next("stream");
    assert_eq!(
        (got["stream_id"].as_str(), got["seq"].as_u64()),
        (Some(viewer_id.as_str()), Some(1))
    );
    assert_eq!(got["data_b64"], b64(b"hello"));
    // Closing is seen on both ends.
    assert_eq!(
        a.ask(json!({"id": 1, "op": "stream.close", "stream_id": owner_id}))["ok"],
        true
    );
    assert_eq!(sb.next("stream_closed")["stream_id"], viewer_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn teammate_without_grant_gets_no_grant() {
    let (a, b) = pair(&[]);
    let r = b.ask(json!({"id": 1, "op": "stream.open", "session": c1(&a)}));
    assert_eq!(r["error"], "no_grant", "{r}");
}

/// A team whose founder runs no daemon, admitting `members` in order (each `extra` over a
/// computer member's fields), seeded into `envs`.
fn team(envs: &[&Env], members: &[(&Identity, &str, Value)]) {
    let f = identity();
    let mut log = vec![net_ops::genesis(&f, "founder", "fmac", "acme", 1)];
    for (who, name, extra) in members {
        let e = admit(&f, log.last().unwrap(), who, name, extra.clone());
        log.push(e);
    }
    seed(envs, &log);
}

/// The member `m`, played by the test, connected to `to` (online there once this returns).
async fn join(m: &Identity, to: &Env) -> (Endpoint, Peer) {
    let ep = endpoint(m).await;
    let mut peer = Peer::dial(&ep, to).await;
    peer.hello(serde_json::from_value(to.heads()).unwrap())
        .await;
    until(10, "the played member is online", || {
        let r = to.ask(json!({"id": 1, "op": "team.get"}));
        r["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["key"] == m.key && x["online"] == true)
    });
    (ep, peer)
}

fn stored_grants(e: &Env) -> i64 {
    let db = rusqlite::Connection::open(e.state.path().join("state.db")).unwrap();
    db.query_row("SELECT count(*) FROM grants", [], |r| r.get(0))
        .unwrap()
}

fn unsigned_grant(session: String, viewer: &str, granter: &str, expires_at: i64) -> Frame {
    Frame::ShareGrant {
        grant_id: ulid(),
        session,
        viewer: viewer.into(),
        input: false,
        expires_at,
        granter: granter.into(),
        sig: String::new(),
    }
}

/// B (a daemon) and M (played, connected to B) in a team with A (identity only); `make`
/// builds the grant M sends from `(A, B, M)`. Returns B (once it has dropped or stored it),
/// A, and M's connection, kept open.
async fn deliver(
    make: impl FnOnce(&Identity, &Identity, &Identity) -> Frame,
) -> (Env, Identity, (Endpoint, Peer)) {
    let b = Env::new();
    let (a, m) = (identity(), identity());
    team(
        &[&b],
        &[
            (&a, "prasiddha", json!({})),
            (&b.identity(), "bob", json!({})),
            (&m, "mal", json!({})),
        ],
    );
    b.daemon(false, "", &[]);
    let (ep, mut peer) = join(&m, &b).await;
    peer.send(&make(&a, &b.identity(), &m)).await;
    until(10, "B drops or stores the grant", || {
        dropped(&b, "share_grant") + stored_grants(&b) as u64 == 1
    });
    (b, a, (ep, peer))
}

async fn assert_dropped(make: impl FnOnce(&Identity, &Identity, &Identity) -> Frame) {
    let (b, a, _m) = deliver(make).await;
    assert_eq!((dropped(&b, "share_grant"), stored_grants(&b)), (1, 0));
    let r =
        b.ask(json!({"id": 1, "op": "stream.open", "session": format!("{}/codex:c1", a.short)}));
    assert_eq!(r["error"], "no_grant", "{r}");
}

fn own_session(id: &Identity) -> String {
    format!("{}/codex:c1", id.short)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forged_grant_for_another_members_session_is_dropped() {
    // Only the session's prefix is wrong: M grants A's session.
    assert_dropped(|a, b, m| {
        signed(
            m,
            unsigned_grant(own_session(a), &b.key, &m.key, now() + 3600),
        )
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grant_with_a_bad_signature_is_dropped() {
    assert_dropped(|_, b, m| {
        let g = unsigned_grant(own_session(m), &b.key, &m.key, now() + 3600);
        signed(&identity(), g)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grant_relayed_by_another_member_is_dropped() {
    // A's own, validly signed grant to B, but M is the one who sends it.
    assert_dropped(|a, b, _| {
        signed(
            a,
            unsigned_grant(own_session(a), &b.key, &a.key, now() + 3600),
        )
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grant_naming_another_viewer_is_dropped() {
    assert_dropped(|a, _, m| {
        signed(
            m,
            unsigned_grant(own_session(m), &a.key, &m.key, now() + 3600),
        )
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grant_ending_past_seven_days_is_dropped() {
    assert_dropped(|_, b, m| {
        let end = now() + 8 * 86_400;
        signed(m, unsigned_grant(own_session(m), &b.key, &m.key, end))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_grant_is_dropped() {
    assert_dropped(|_, b, m| signed(m, unsigned_grant(own_session(m), &b.key, &m.key, now() - 1)))
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn valid_grant_is_stored_and_a_silent_owner_is_unreachable() {
    let mut m_session = String::new();
    let (b, _, _m) = deliver(|_, b, m| {
        m_session = own_session(m);
        signed(
            m,
            unsigned_grant(own_session(m), &b.key, &m.key, now() + 3600),
        )
    })
    .await;
    assert_eq!((dropped(&b, "share_grant"), stored_grants(&b)), (0, 1));
    // M never answers the stream: after 10 s that is `unreachable`, not a refusal.
    let r = b.ask(json!({"id": 1, "op": "stream.open", "session": m_session}));
    assert_eq!(r["error"], "unreachable", "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn granted_viewer_receives_bytes_in_order_until_expiry() {
    let (a, b) = pair(&[]);
    let (sa, sb) = (Sub::new(&a), Sub::new(&b));
    let expires_at = now() + 4;
    let r = grant(&a, &c1(&a), &b.identity().key, false, expires_at);
    assert_eq!(r["ok"], true, "{r}");
    let viewer_id = open(&b, &c1(&a));
    let owner_id: String = sa.next("stream_opened")["stream_id"]
        .as_str()
        .unwrap()
        .into();
    for chunk in ["one", "two", "three"] {
        assert_eq!(send(&a, &owner_id, chunk.as_bytes())["ok"], true);
    }
    for (seq, chunk) in [(1, "one"), (2, "two"), (3, "three")] {
        let got = sb.next("stream");
        assert_eq!(got["stream_id"], viewer_id);
        assert_eq!(got["seq"], seq);
        assert_eq!(got["data_b64"], b64(chunk.as_bytes()));
    }
    // Past the grant's end, the owner's next send closes the stream.
    std::thread::sleep(Duration::from_secs((expires_at - now() + 1).max(0) as u64));
    assert_eq!(send(&a, &owner_id, b"late")["error"], "no_grant");
    assert_eq!(sb.next("stream_closed")["stream_id"], viewer_id);
    let r = b.ask(json!({"id": 1, "op": "stream.open", "session": c1(&a)}));
    assert_eq!(r["error"], "no_grant", "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn viewer_bytes_never_reach_the_owner() {
    let (a, b) = pair(&[]);
    let sa = Sub::new(&a);
    let r = grant(&a, &c1(&a), &b.identity().key, false, now() + 600);
    assert_eq!(r["ok"], true, "{r}");
    let viewer_id = open(&b, &c1(&a));
    sa.next("stream_opened");
    assert_eq!(send(&b, &viewer_id, b"rm -rf /")["ok"], true);
    until(10, "A drops the viewer's frame", || {
        dropped(&a, "input") == 1
    });
    assert!(!sa.any("stream_input", 300));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn own_bridge_input_grant_delivers_viewer_frames() {
    // F founds; A (the owner, workspace "test") and C (the workspace's cloud member) join.
    let (f, a, c) = (Env::new(), Env::new(), Env::new());
    let (i_f, ia, ic) = (f.identity(), a.identity(), c.identity());
    let g = net_ops::genesis(&i_f, "founder", "fmac", "acme", 1);
    let fa = admit(&i_f, &g, &ia, "prasiddha", json!({"workspace": "test"}));
    let fc = admit(
        &i_f,
        &fa,
        &ic,
        "bridge",
        json!({"kind": "cloud", "person_id": "ws:test", "proof": "p"}),
    );
    // Another workspace's cloud member (no daemon).
    let other = identity();
    let fo = admit(
        &i_f,
        &fc,
        &other,
        "elsewhere",
        json!({"kind": "cloud", "person_id": "ws:other", "proof": "p"}),
    );
    seed(&[&f, &a, &c], &[g, fa, fc, fo]);
    f.daemon(false, "", &[]);
    c.daemon(false, "", &[]);
    a.daemon(false, "", &[peers(&[&f, &c])]);
    until(10, "A sees both", || a.status()["peers"]["online"] == 2);

    let sa = Sub::new(&a);
    for viewer in [&i_f.key, &other.key] {
        let r = grant(&a, &c1(&a), viewer, true, now() + 600);
        assert_eq!(r["error"], "unsupported", "{r}");
    }
    let r = grant(&a, &c1(&a), &ic.key, true, now() + 600);
    assert_eq!(r["ok"], true, "{r}");
    let viewer_id = open(&c, &c1(&a));
    let owner_id = sa.next("stream_opened")["stream_id"].clone();
    assert_eq!(send(&c, &viewer_id, b"ls\n")["ok"], true);
    let got = sa.next("stream_input");
    assert_eq!(got["stream_id"], owner_id);
    assert_eq!(got["seq"], 1);
    assert_eq!(got["data_b64"], b64(b"ls\n"));
    // Input frames are at most 4 KB.
    assert_eq!(send(&c, &viewer_id, &[b'x'; 4097])["error"], "bad_request");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chunk_over_64k_is_bad_request() {
    let (a, b) = same_person();
    let sa = Sub::new(&a);
    open(&b, &c1(&a));
    let owner_id: String = sa.next("stream_opened")["stream_id"]
        .as_str()
        .unwrap()
        .into();
    assert_eq!(
        send(&a, &owner_id, &vec![0; 65_537])["error"],
        "bad_request"
    );
    assert_eq!(send(&a, &owner_id, &vec![0; 65_536])["ok"], true);
}

/// A stream to the owner from the played member: `Ok` when the owner echoes the opener,
/// `Err(true)` when it drops the stream, `Err(false)` when it stays silent for 5 s.
async fn raw_open(
    peer: &Peer,
    grant_id: &str,
    session: &str,
) -> Result<(SendStream, RecvStream), bool> {
    let (mut s, mut r) = peer.conn.open_bi().await.unwrap();
    frames::write(&mut s, &streams::opener(grant_id, session))
        .await
        .unwrap();
    match tokio::time::timeout(Duration::from_secs(5), frames::read(&mut r)).await {
        Ok(Ok(Frame::Stream { seq: 0, .. })) => Ok((s, r)),
        Ok(_) => Err(true),
        Err(_) => Err(false),
    }
}

/// A (the owner, a daemon, workspace "test") and M (played, connected to A), M admitted with
/// `m_extra(A)` over a computer member's fields.
async fn owner_and(m_extra: impl FnOnce(&Identity) -> Value) -> (Env, Identity, (Endpoint, Peer)) {
    let (a, m) = (Env::new(), identity());
    let ia = a.identity();
    team(
        &[&a],
        &[
            (&ia, "prasiddha", json!({"workspace": "test"})),
            (&m, "mal", m_extra(&ia)),
        ],
    );
    a.daemon(false, "", &[]);
    let j = join(&m, &a).await;
    (a, m, j)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_refuses_a_stream_without_a_grant() {
    let (a, _m, (_ep, peer)) = owner_and(|_| json!({})).await;
    let sa = Sub::new(&a);
    assert_eq!(raw_open(&peer, "", &c1(&a)).await.err(), Some(true));
    assert!(!sa.any("stream_opened", 300));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_refuses_another_viewers_grant() {
    let (a, b, m) = (Env::new(), Env::new(), identity());
    team(
        &[&a, &b],
        &[
            (&a.identity(), "prasiddha", json!({})),
            (&b.identity(), "bob", json!({})),
            (&m, "mal", json!({})),
        ],
    );
    b.daemon(false, "", &[]);
    a.daemon(false, "", &[peers(&[&b])]);
    until(10, "A and B online", || a.status()["peers"]["online"] == 1);
    let (_ep, peer) = join(&m, &a).await;
    let r = grant(&a, &c1(&a), &b.identity().key, false, now() + 600);
    let grant_id = r["grant_id"].as_str().unwrap();
    let sa = Sub::new(&a);
    assert_eq!(raw_open(&peer, grant_id, &c1(&a)).await.err(), Some(true));
    assert!(!sa.any("stream_opened", 300));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_refuses_a_cloud_member_with_my_person_id() {
    let (a, _m, (_ep, peer)) =
        owner_and(|a| json!({"kind": "cloud", "person_id": a.key, "proof": "p"})).await;
    let sa = Sub::new(&a);
    assert_eq!(raw_open(&peer, "", &c1(&a)).await.err(), Some(true));
    assert!(!sa.any("stream_opened", 300));
}

/// A and its workspace member M (played), M's input grant ending in `secs`, M's stream open:
/// A, M's connection, the stream, A's events, the grant's end.
async fn input_stream(secs: i64) -> (Env, (Endpoint, Peer), SendStream, RecvStream, Sub, i64) {
    let (a, m, j) =
        owner_and(|_| json!({"kind": "cloud", "person_id": "ws:test", "proof": "p"})).await;
    let end = now() + secs;
    let r = grant(&a, &c1(&a), &m.key, true, end);
    assert_eq!(r["ok"], true, "{r}");
    let sa = Sub::new(&a);
    let grant_id = r["grant_id"].as_str().unwrap();
    let (s, rv) = raw_open(&j.1, grant_id, &c1(&a)).await.unwrap();
    sa.next("stream_opened");
    (a, j, s, rv, sa, end)
}

fn input(a: &Env, seq: u32, n: usize) -> Frame {
    Frame::Stream {
        grant_id: String::new(),
        session: c1(a),
        seq,
        bytes: b64(&vec![b'x'; n]),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_caps_input_at_4k_and_counts_other_frames() {
    let (a, _j, mut s, _r, sa, _) = input_stream(600).await;
    frames::write(&mut s, &input(&a, 1, 4097)).await.unwrap();
    let hello = Frame::Hello {
        protocol: 1,
        heads: vec![],
    };
    frames::write(&mut s, &hello).await.unwrap();
    frames::write(&mut s, &input(&a, 3, 4096)).await.unwrap();
    let got = sa.next("stream_input");
    assert_eq!(got["seq"], 3);
    assert_eq!(got["data_b64"], b64(&[b'x'; 4096]));
    assert_eq!(dropped(&a, "input"), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_input_grant_never_raises_input() {
    let (a, _j, mut s, _r, sa, end) = input_stream(3).await;
    std::thread::sleep(Duration::from_secs((end - now() + 1).max(0) as u64));
    // The owner closed the stream at the grant's end, so this write may be refused.
    let _ = frames::write(&mut s, &input(&a, 1, 2)).await;
    loop {
        let v =
            sa.0.recv_timeout(Duration::from_secs(10))
                .expect("an event");
        assert_ne!(v["event"], "stream_input", "{v}");
        if v["event"] == "stream_closed" {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_unblocks_a_send_waiting_on_flow_control() {
    // M is the owner's own computer (no grant needed) and never reads its stream.
    let (a, _m, (_ep, peer)) = owner_and(|a| json!({"person_id": a.key, "proof": "t"})).await;
    let sa = Sub::new(&a);
    let (_s, _r) = raw_open(&peer, "", &c1(&a)).await.unwrap();
    let owner_id: String = sa.next("stream_opened")["stream_id"]
        .as_str()
        .unwrap()
        .into();
    let (tx, rx) = mpsc::channel();
    // Detached, so a send that never returns fails the test instead of hanging it.
    let (sock, id) = (a.state.path().join("sock"), owner_id.clone());
    std::thread::spawn(move || {
        let chunk = b64(&[0; 65_536]);
        for _ in 0..100 {
            let mut c = UnixStream::connect(&sock).unwrap();
            let req = json!({"id": 1, "op": "stream.send", "stream_id": id, "data_b64": chunk});
            writeln!(c, "{req}").unwrap();
            let mut line = String::new();
            BufReader::new(c).read_line(&mut line).unwrap();
            let r: Value = serde_json::from_str(&line).unwrap();
            let ok = r["ok"] == true;
            if tx.send(r).is_err() || !ok {
                return;
            }
        }
    });
    let mut sent = 0;
    // A send that has not returned in 5 s is stalled on flow control.
    while let Ok(r) = rx.recv_timeout(Duration::from_secs(5)) {
        assert_eq!(r["ok"], true, "{r}");
        sent += 1;
    }
    assert!((1..100).contains(&sent), "{sent} sends before one stalled");
    let t = Instant::now();
    let r = a.ask(json!({"id": 1, "op": "stream.close", "stream_id": owner_id}));
    assert_eq!(r["ok"], true, "{r}");
    assert!(t.elapsed() < Duration::from_secs(5));
    let blocked = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the blocked send returns");
    assert_eq!(blocked["ok"], false, "{blocked}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_input_at_or_after_the_grants_end() {
    let (a, _j, mut s, _r, sa, end) = input_stream(2).await;
    // Each frame carries its send time (Unix ms); one every 50 ms until a second past the end.
    let mut seq = 1;
    while now_ms() < end as u64 * 1000 + 1000 {
        let f = Frame::Stream {
            grant_id: String::new(),
            session: c1(&a),
            seq,
            bytes: b64(now_ms().to_string().as_bytes()),
        };
        if frames::write(&mut s, &f).await.is_err() {
            break; // closed at the end
        }
        seq += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut last = 0;
    loop {
        let v =
            sa.0.recv_timeout(Duration::from_secs(10))
                .expect("an event");
        if v["event"] == "stream_input" {
            let sent = data_encoding::BASE64
                .decode(v["data_b64"].as_str().unwrap().as_bytes())
                .unwrap();
            last = String::from_utf8(sent).unwrap().parse().unwrap();
        }
        if v["event"] == "stream_closed" {
            break;
        }
    }
    assert!(last > 0, "some input before the end");
    assert!(
        last < end as u64 * 1000,
        "input sent at {last}, the grant ended at {end}"
    );
}

/// Unix milliseconds.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
