//! Streams: a viewer watching a session's live bytes over its own QUIC stream, on a signed
//! `share_grant` (or as the same person's computer). Real daemons; the forged grants come from
//! a member the test plays (`Peer`).

use lightbulb_connect::daemon::net_ops;
use lightbulb_connect::identity::Identity;
use lightbulb_connect::net::frames::Frame;
use lightbulb_connect::roster::{self, Entry, Kind};
use lightbulb_connect::streams;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod common;

use common::{endpoint, identity, pair, peers, person, start_pair, ulid, until, Env, Peer};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn session(e: &Env) -> String {
    format!("{}/codex:c1", e.short())
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
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(s).lines() {
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
        let deadline = std::time::Instant::now() + Duration::from_millis(ms);
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
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
    e.status()["dropped"][what].as_u64().unwrap()
}

/// `who` admitted by `by` under `parent`, with `body`'s fields over a computer member's.
fn admit(by: &Identity, parent: &Entry, who: &Identity, name: &str, extra: Value) -> Entry {
    let mut body = json!({"member": who.key, "name": name, "computer": name,
        "person_id": who.key, "kind": "computer", "role": "member"});
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    Entry::signed(by, vec![parent.hash()], Kind::Admit, body, 2)
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
    let viewer_id = open(&b, &session(&a));
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
    let r = b.ask(json!({"id": 1, "op": "stream.open", "session": session(&a)}));
    assert_eq!(r["error"], "no_grant", "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forged_grant_for_another_members_session_is_dropped() {
    let (a, b) = (Env::new(), Env::new());
    let m = identity();
    let [_, ab] = common::seed_pair(&a, &b);
    let am = admit(&a.identity(), &ab, &m, "mal", json!({}));
    seed(&[&a, &b], &[am]);
    start_pair(&a, &b, "", &[]);
    let (ka, kb) = (a.identity().key, b.identity().key);

    let ep = endpoint(&m).await;
    let mut peer = Peer::dial(&ep, &b).await;
    peer.hello(serde_json::from_value(b.heads()).unwrap()).await;
    let forged = |session: String, viewer: &str, granter: &str| {
        let f = Frame::ShareGrant {
            grant_id: ulid(),
            session,
            viewer: viewer.into(),
            input: false,
            expires_at: now() + 3600,
            granter: granter.into(),
            sig: String::new(),
        };
        streams::signed(&m, f)
    };
    // M grants A's session (not its own), claims to be A, and names itself as the viewer.
    for f in [
        forged(session(&a), &kb, &m.key),
        forged(session(&a), &kb, &ka),
        forged(format!("{}/codex:c1", m.short), &m.key, &m.key),
    ] {
        peer.send(&f).await;
    }
    until(10, "B drops all three", || dropped(&b, "grants") == 3);
    let db = rusqlite::Connection::open(b.state.path().join("state.db")).unwrap();
    let n: i64 = db
        .query_row("SELECT count(*) FROM grants", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
    let r = b.ask(json!({"id": 1, "op": "stream.open", "session": session(&a)}));
    assert_eq!(r["error"], "no_grant", "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn granted_viewer_receives_bytes_in_order_until_expiry() {
    let (a, b) = pair(&[]);
    let (sa, sb) = (Sub::new(&a), Sub::new(&b));
    let expires_at = now() + 4;
    let r = grant(&a, &session(&a), &b.identity().key, false, expires_at);
    assert_eq!(r["ok"], true, "{r}");
    let viewer_id = open(&b, &session(&a));
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
    let r = b.ask(json!({"id": 1, "op": "stream.open", "session": session(&a)}));
    assert_eq!(r["error"], "no_grant", "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn viewer_bytes_never_reach_the_owner() {
    let (a, b) = pair(&[]);
    let sa = Sub::new(&a);
    let r = grant(&a, &session(&a), &b.identity().key, false, now() + 600);
    assert_eq!(r["ok"], true, "{r}");
    let viewer_id = open(&b, &session(&a));
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
    seed(&[&f, &a, &c], &[g, fa, fc]);
    f.daemon(false, "", &[]);
    c.daemon(false, "", &[]);
    a.daemon(false, "", &[peers(&[&f, &c])]);
    until(10, "A sees both", || a.status()["peers"]["online"] == 2);

    let sa = Sub::new(&a);
    let r = grant(&a, &session(&a), &i_f.key, true, now() + 600);
    assert_eq!(r["error"], "unsupported", "{r}");
    let r = grant(&a, &session(&a), &ic.key, true, now() + 600);
    assert_eq!(r["ok"], true, "{r}");
    let viewer_id = open(&c, &session(&a));
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
    open(&b, &session(&a));
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
