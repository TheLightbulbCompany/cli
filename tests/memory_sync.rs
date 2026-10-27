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
