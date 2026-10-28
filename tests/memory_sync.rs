//! Memory entries replicate between members behind a `hello` capability: heads, paged frames,
//! the `me` scope and a removed member. Real daemons; a member played by the test where it
//! needs to see frames.

use lightbulb::daemon::net_ops;
use lightbulb::memory::{self, Entry, Kind, Op, Scope, Source};
use lightbulb::net::frames::Frame;
use lightbulb::net::ALPN;
use lightbulb::roster;
use serde_json::json;
use std::cell::RefCell;
use std::time::Duration;

mod common;

use common::{endpoint, seed_pair, session, until, Env, Peer, EXE};

const PAGE_BYTES: usize = 512 * 1024;

fn op(subject: &str, fact: &str, scope: Scope) -> Op {
    Op::Remember {
        subject: subject.into(),
        fact: fact.into(),
        kind: Kind::Fact,
        scope,
        when: None,
        source: Source {
            harness: "test".into(),
            repo: None,
            session: None,
            on_behalf_of: None,
            shared_from: None,
        },
        replaces: vec![],
    }
}

/// `env`'s computer remembers `op`.
fn remember(env: &Env, op: Op) -> Entry {
    memory::remember(&env.store(), &env.identity(), op, 5).unwrap()
}

/// What `env` holds of `author`.
fn held(env: &Env, author: &str) -> Vec<Entry> {
    memory::since(&env.store(), author, 0, 1_000).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn team_entry_reaches_the_other_member() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    let from_a = remember(&a, op("db", "postgres 16", Scope::Team));
    let from_b = remember(&b, op("ci", "github actions", Scope::Team));
    common::start_pair(&a, &b, "", &[]);
    let (ka, kb) = (a.identity().key, b.identity().key);
    until(10, "each holds the other's entry", || {
        held(&b, &ka) == [from_a.clone()] && held(&a, &kb) == [from_b.clone()]
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_me_scope_syncs_only_same_person() {
    // A and B are signed in as one person, C as another; one team of three.
    let (a, b, c) = (Env::new(), Env::new(), Env::new());
    let (ia, ib, ic) = (a.identity(), b.identity(), c.identity());
    let g = net_ops::genesis(&ia, "prasiddha", "mac", "acme", 1);
    let ab = common::admit(&ia, &g, &ib, "bob", json!({"computer": "mbp"}));
    let ac = common::admit(&ia, &ab, &ic, "cy", json!({"computer": "imac"}));
    for env in [&a, &b, &c] {
        let store = env.store();
        for e in [&g, &ab, &ac] {
            roster::accept(&store, e, 10).unwrap();
        }
    }
    a.sign_in("user_one", None);
    b.sign_in("user_one", None);
    c.sign_in("user_two", None);
    let mine = remember(&a, op("mine", "private", Scope::Me));
    let ours = remember(&a, op("ours", "shared", Scope::Team));
    b.daemon(false, "", &[]);
    c.daemon(false, "", &[]);
    a.daemon(false, "", &[common::peers(&[&b, &c])]);
    let ka = ia.key;
    until(
        10,
        "both entries reach B, the other person's computer",
        || held(&b, &ka) == [mine.clone(), ours.clone()],
    );
    until(10, "the team entry reaches C", || !held(&c, &ka).is_empty());
    // C is connected and caught up: it was sent the team entry only.
    assert_eq!(held(&c, &ka), [ours]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn entry_written_on_live_link_arrives_without_reconnect() {
    let (a, b) = common::pair(&[]);
    let ka = a.identity().key;
    assert!(held(&b, &ka).is_empty());
    let e = remember(&a, op("db", "postgres 16", Scope::Team));
    until(10, "B holds it on the same link", || {
        held(&b, &ka) == [e.clone()]
    });
    assert_eq!(a.status()["peers"]["online"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_peer_without_capability_gets_no_memory_frames_and_keeps_mail() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    remember(&a, op("db", "postgres 16", Scope::Team));
    a.daemon(false, "", &[]);
    let ep = endpoint(&b.identity()).await;
    let mut old = Peer::dial(&ep, &a).await;
    old.hello(vec![]).await;
    until(5, "B online", || a.status()["peers"]["online"] == 1);
    let (sa, sb) = (session(&a, "a1"), session(&b, "b1"));
    a.send(&sa, &sb, "still here", "r1", None);
    let seen = RefCell::new(vec![]);
    let mail = old
        .recv_where(|f| {
            seen.borrow_mut().push(f.kind());
            matches!(f, Frame::Envelope { .. })
        })
        .await;
    assert!(matches!(mail, Frame::Envelope { body, .. } if body == "still here"));
    let seen = seen.into_inner();
    assert!(seen.contains(&"presence"), "{seen:?}");
    assert!(
        !seen.iter().any(|k| k.starts_with("memory")),
        "memory frames to an old peer: {seen:?}"
    );

    // The same hello with the capability is told our heads (before the presence).
    let ep2 = endpoint(&b.identity()).await;
    let mut new = Peer::dial(&ep2, &a).await;
    new.hello_full(vec![], None, true).await;
    let heads = new
        .recv_where(|f| matches!(f, Frame::MemoryHeads { .. }))
        .await;
    assert!(matches!(heads, Frame::MemoryHeads { heads } if heads.len() == 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_removed_member_entries_refused() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    let (ka, kb) = (a.identity().key, b.identity().key);
    let before = remember(&a, op("db", "postgres 16", Scope::Team));
    common::start_pair(&a, &b, "", &[]);
    until(10, "B holds it while a member", || {
        held(&b, &ka) == [before.clone()]
    });
    let o = a.detached(&[EXE, "remove", "bob"], "");
    assert!(o.status.success(), "{}", common::err(&o));
    until(10, "B is gone", || a.status()["peers"]["online"] == 0);

    let theirs = remember(&b, op("ci", "github actions", Scope::Team));
    let after = remember(&a, op("deploy", "friday", Scope::Team));
    // B redials as itself: refused at the gate, its frames never read.
    let ep = endpoint(&b.identity()).await;
    if let Ok(conn) = ep.connect(common::addr(&a), ALPN).await {
        tokio::time::timeout(Duration::from_secs(3), conn.closed())
            .await
            .expect("redial closed");
    }
    // A would not store B's entry were it offered, and holds none of B's.
    assert!(memory::accept(&a.store(), &theirs, 10).is_err());
    assert!(held(&a, &kb).is_empty());
    // B was sent nothing of A's after the removal.
    assert!(!held(&b, &ka).contains(&after));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gap_of_300_entries_pages() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    for i in 0..300 {
        let fact = format!("{i} {}", "x".repeat(4_000));
        remember(&a, op(&format!("s{i}"), &fact, Scope::Team));
    }
    a.daemon(false, "", &[]);
    let ep = endpoint(&b.identity()).await;
    let mut p = Peer::dial(&ep, &a).await;
    p.hello_full(vec![], None, true).await;
    p.send(&Frame::MemoryHeads {
        heads: Default::default(),
    })
    .await;
    let (mut sizes, mut got, mut mores) = (vec![], vec![], vec![]);
    loop {
        let f = p.recv_where(|f| matches!(f, Frame::Memory { .. })).await;
        sizes.push(serde_json::to_vec(&f).unwrap().len());
        let Frame::Memory { entries, more } = f else {
            unreachable!()
        };
        got.extend(entries.into_iter().map(|e| e.seq));
        mores.push(more);
        if !more {
            break;
        }
    }
    assert_eq!(got, (1..=300).collect::<Vec<u64>>());
    assert!(mores.len() >= 2, "{mores:?}");
    assert!(mores[..mores.len() - 1].iter().all(|m| *m));
    assert!(sizes.iter().all(|n| *n <= PAGE_BYTES), "{sizes:?}");
}

/// How many times `env`'s daemon log says it sent `MemoryHeads`.
fn heads_sent(env: &Env) -> usize {
    std::fs::read_to_string(env.state.path().join("daemon.log"))
        .unwrap()
        .matches("net: sent memory heads")
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn me_latest_on_both_sides_does_not_ping_pong() {
    // Two people, each whose newest entry is `me`: neither head can ever reach the other.
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    a.sign_in("user_one", None);
    b.sign_in("user_two", None);
    remember(&a, op("ta", "team a", Scope::Team));
    remember(&a, op("ma", "mine a", Scope::Me));
    remember(&b, op("tb", "team b", Scope::Team));
    remember(&b, op("mb", "mine b", Scope::Me));
    common::start_pair(&a, &b, "", &[]);
    let (ka, kb) = (a.identity().key, b.identity().key);
    until(10, "team entries cross", || {
        held(&b, &ka).len() == 1 && held(&a, &kb).len() == 1
    });
    std::thread::sleep(Duration::from_secs(3));
    let (na, nb) = (heads_sent(&a), heads_sent(&b));
    assert!((1..=3).contains(&na) && (1..=3).contains(&nb), "{na} {nb}");
    assert_eq!((held(&b, &ka).len(), held(&a, &kb).len()), (1, 1));
}

/// A node restored to an older state.db (its key kept) is told by a peer how far it got, and
/// numbers its next entry past that, so the peer stores it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restored_node_numbers_past_what_peers_hold() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    let ka = a.identity().key;
    // B holds A's first three; A then loses them (an older state.db restored).
    for i in 0..3 {
        let e = remember(&a, op(&format!("s{i}"), "x", Scope::Team));
        assert!(memory::accept(&b.store(), &e, 10).unwrap());
    }
    rusqlite::Connection::open(a.state.path().join("state.db"))
        .unwrap()
        .execute("DELETE FROM memory_entries", [])
        .unwrap();
    a.daemon(false, "", &[]);
    let ep = endpoint(&b.identity()).await;
    let mut p = Peer::dial(&ep, &a).await;
    p.hello_full(vec![], None, true).await;
    p.send(&Frame::MemoryHeads {
        heads: memory::heads(&b.store()).unwrap(),
    })
    .await;
    until(5, "A records what B holds of it", || {
        common::count(
            &a.state.path().join("state.db"),
            "SELECT COALESCE(MAX(seq), 0) FROM memory_seen",
        ) == 3
    });
    let next = remember(&a, op("s3", "after the restore", Scope::Team));
    assert_eq!(next.seq, 4);
    assert!(
        memory::accept(&b.store(), &next, 10).unwrap(),
        "B stores it"
    );
    assert_eq!(held(&b, &ka).len(), 4);
}

/// An entry with a credential in it, signed by `env` as its first: what an older build, which
/// did not refuse it on `remember`, could send.
fn with_secret(env: &Env) -> Entry {
    use data_encoding::HEXLOWER;
    use sha2::{Digest, Sha256};
    let id = env.identity();
    let mut e = Entry {
        v: 1,
        id: String::new(),
        author: id.key.clone(),
        seq: 1,
        op: op("deploy", "export API_KEY=abcd1234efgh5678", Scope::Team),
        at: 5,
        sig: String::new(),
    };
    let mut v = serde_json::to_value(&e).unwrap();
    v.as_object_mut().unwrap().remove("sig");
    e.id = HEXLOWER.encode(&Sha256::digest(lightbulb::identity::canonical(&v)));
    e.sig = id.sign(&serde_json::to_value(&e).unwrap());
    e
}

#[test]
fn an_entry_from_a_peer_with_a_secret_is_refused_on_accept() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    let e = with_secret(&a);
    let kb = b.state.path().join("state.db");
    // Stored as a tombstone: its seq is held, its text is not, and the reason is kept.
    assert!(memory::accept(&b.store(), &e, 10).unwrap());
    assert!(!memory::accept(&b.store(), &e, 10).unwrap(), "already held");
    assert_eq!(memory::heads(&b.store()).unwrap()[&e.author], 1);
    assert!(held(&b, &e.author).is_empty());
    assert!(memory::facts(&b.store(), None, None).unwrap().is_empty());
    let (body, tomb, reason): (Option<String>, i64, Option<String>) =
        rusqlite::Connection::open(kb)
            .unwrap()
            .query_row(
                "SELECT entry, tombstone, reason FROM memory_entries WHERE id = ?1",
                [&e.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
    assert_eq!(
        (body, tomb, reason.as_deref()),
        (None, 1, Some("refused:credential"))
    );
    // The author's next entry, numbered past it, is stored after it.
    assert!(memory::accept(&a.store(), &e, 10).unwrap());
    let next = remember(&a, op("ci", "github actions", Scope::Team));
    assert!(memory::accept(&b.store(), &next, 10).unwrap());
}
