//! `memory`: signed entries, replay, the FTS5 index and the secret filter.

mod common;

use common::identity;
use lightbulb::identity::Identity;
use lightbulb::memory::{self, Kind, Op, Scope, Source, Status};
use lightbulb::roster::{self, Kind as RosterKind};
use lightbulb::store::Store;
use serde_json::json;

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(&dir.path().join("state.db")).unwrap();
    (dir, s)
}

/// A store whose roster admits `who` (as founder).
fn team(who: &Identity) -> (tempfile::TempDir, Store) {
    let (d, s) = store();
    let g = roster::Entry::signed(
        who,
        vec![],
        RosterKind::Admit,
        json!({"member": who.key, "name": "a", "computer": "mac", "role": "founder",
            "ticket_id": "", "team_name": "acme"}),
        1,
    );
    roster::accept(&s, &g, 10).unwrap();
    (d, s)
}

fn rem(subject: &str, fact: &str, replaces: &[&str]) -> Op {
    Op::Remember {
        subject: subject.into(),
        fact: fact.into(),
        kind: Kind::Fact,
        scope: Scope::Team,
        when: None,
        source: Source {
            harness: "test".into(),
            repo: Some("acme/app".into()),
            session: None,
            on_behalf_of: None,
            shared_from: None,
        },
        replaces: replaces.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn remember_assigns_increasing_seq_and_verifies() {
    let a = identity();
    let (_d1, s1) = team(&a);
    let e1 = memory::remember(&s1, &a, rem("db", "postgres 16", &[]), 5).unwrap();
    let e2 = memory::remember(&s1, &a, rem("ci", "github actions", &[]), 6).unwrap();
    assert_eq!((e1.seq, e2.seq), (1, 2));
    assert_eq!(memory::heads(&s1).unwrap()[&a.key], 2);

    let (_d2, s2) = team(&a);
    assert!(memory::accept(&s2, &e1, 7).unwrap());
    assert!(!memory::accept(&s2, &e1, 7).unwrap(), "already held");
    let mut bad = e2.clone();
    if let Op::Remember { fact, .. } = &mut bad.op {
        *fact = "jenkins".into();
    }
    assert!(memory::accept(&s2, &bad, 7).is_err(), "tampered");
    assert!(memory::accept(&s2, &e2, 7).unwrap());
    assert_eq!(memory::since(&s2, &a.key, 1, 10).unwrap(), vec![e2]);

    let stranger = identity();
    let (_d3, s3) = store();
    let e = memory::remember(&s3, &stranger, rem("x", "y", &[]), 5).unwrap();
    assert!(memory::accept(&s2, &e, 7).is_err(), "not a member");
}

#[test]
fn replace_chain_yields_one_current_with_history() {
    let a = identity();
    let (_d, s) = team(&a);
    let e1 = memory::remember(&s, &a, rem("db", "mysql", &[]), 1).unwrap();
    let e2 = memory::remember(&s, &a, rem("db", "postgres 15", &[&e1.id]), 2).unwrap();
    let e3 = memory::remember(&s, &a, rem("db", "postgres 16", &[&e2.id]), 3).unwrap();
    let f = memory::facts(&s, None, None).unwrap();
    assert_eq!(f.len(), 1);
    assert_eq!(
        (f[0].id.as_str(), f[0].status),
        (e3.id.as_str(), Status::Current)
    );
    assert_eq!(f[0].value, "postgres 16");
    let hist: Vec<&str> = f[0].history.iter().map(|h| h.1.as_str()).collect();
    assert_eq!(hist, ["mysql", "postgres 15"]);
}

#[test]
fn two_replaces_of_same_target_are_contested() {
    let (a, b) = (identity(), identity());
    let (_d, s) = team(&a);
    let e1 = memory::remember(&s, &a, rem("db", "mysql", &[]), 1).unwrap();
    memory::remember(&s, &a, rem("db", "postgres", &[&e1.id]), 2).unwrap();
    // A second author's entry, built on its own store and accepted here once admitted.
    let (_d2, sb) = store();
    let eb = memory::remember(&sb, &b, rem("db", "sqlite", &[&e1.id]), 3).unwrap();
    let g = roster::entries(&s).unwrap().remove(0);
    let adm = roster::Entry::signed(
        &a,
        vec![g.hash()],
        RosterKind::Admit,
        json!({"member": b.key, "name": "b", "computer": "mac", "role": "member"}),
        2,
    );
    roster::accept(&s, &adm, 10).unwrap();
    assert!(memory::accept(&s, &eb, 10).unwrap());
    let f = memory::facts(&s, None, None).unwrap();
    assert_eq!(f.len(), 2);
    assert!(f.iter().all(|x| x.status == Status::Contested));

    // A same-subject remember with a different fact and no `replaces` contests too.
    let (_d3, s3) = team(&a);
    memory::remember(&s3, &a, rem("ci", "actions", &[]), 1).unwrap();
    memory::remember(&s3, &a, rem("ci", "jenkins", &[]), 2).unwrap();
    let f = memory::facts(&s3, None, None).unwrap();
    assert!(f.len() == 2 && f.iter().all(|x| x.status == Status::Contested));
}

#[test]
fn test_tombstone_before_entry_wins() {
    let a = identity();
    let (_d1, s1) = team(&a);
    let e = memory::remember(&s1, &a, rem("db", "postgres", &[]), 1).unwrap();
    let t = memory::remember(
        &s1,
        &a,
        Op::Forget {
            target: e.id.clone(),
        },
        2,
    )
    .unwrap();
    assert!(memory::facts(&s1, None, None).unwrap().is_empty());

    // Another store sees the tombstone first, then the body.
    let (_d2, s2) = team(&a);
    assert!(memory::accept(&s2, &t, 3).unwrap());
    assert!(memory::accept(&s2, &e, 3).unwrap());
    assert!(memory::facts(&s2, None, None).unwrap().is_empty());
    assert!(memory::search(&s2, "postgres", 5).unwrap().is_empty());
    assert_eq!(memory::heads(&s2).unwrap()[&a.key], 2);
    // The body is gone: what a peer is served for seq 1 is nothing, the forget is seq 2.
    assert_eq!(memory::since(&s2, &a.key, 0, 10).unwrap(), vec![t]);
}

#[test]
fn search_finds_by_subject_and_fact() {
    let a = identity();
    let (_d, s) = team(&a);
    memory::remember(&s, &a, rem("database choice", "we run postgres 16", &[]), 1).unwrap();
    memory::remember(&s, &a, rem("ci", "github actions on every push", &[]), 2).unwrap();
    let by_subject = memory::search(&s, "database", 5).unwrap();
    assert_eq!(by_subject.len(), 1);
    assert_eq!(by_subject[0].subject, "database choice");
    let by_fact = memory::search(&s, "actions", 5).unwrap();
    assert_eq!(by_fact[0].subject, "ci");
    assert!(memory::search(&s, "kubernetes", 5).unwrap().is_empty());
}

/// Agents ask questions: any word of one may match (stemmed, prefix), stopwords aside.
#[test]
fn search_matches_natural_questions() {
    let a = identity();
    let (_d, s) = team(&a);
    let fact = "Run the suite with cargo test --workspace before pushing";
    memory::remember(&s, &a, rem("test runner", fact, &[]), 1).unwrap();
    memory::remember(&s, &a, rem("deploy", "fridays only", &[]), 2).unwrap();
    for q in [
        "How do we run tests before pushing?",
        "tests",
        "cargo test",
        "run the suite",
    ] {
        let hits = memory::search(&s, q, 5).unwrap();
        assert_eq!(hits.len(), 1, "{q}: {hits:?}");
        assert_eq!(hits[0].subject, "test runner", "{q}");
    }
    // Stemmed both ways.
    assert_eq!(
        memory::search(&s, "deploys", 5).unwrap()[0].subject,
        "deploy"
    );
    // FTS5 syntax is text, never an error.
    for q in [r#"cargo" OR "x*"#, "* NEAR( OR", r#"""#, "tests*"] {
        assert!(memory::search(&s, q, 5).is_ok(), "{q}");
    }
    assert_eq!(memory::search(&s, r#"cargo" OR "x*"#, 5).unwrap().len(), 1);
}

/// A peer's claim of how far it holds our entries numbers our next one only when plausible: a
/// restore loses at most a million; anything past that is ignored, never stored.
#[test]
fn an_absurd_seen_head_is_ignored() {
    let a = identity();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let s = Store::open(&path).unwrap();
    let next = |s: &Store, n: i64| memory::remember(s, &a, rem(&format!("s{n}"), "x", &[]), n);
    assert_eq!(next(&s, 1).unwrap().seq, 1);
    assert!(!memory::saw(&s, &a.key, i64::MAX as u64).unwrap());
    assert!(!memory::saw(&s, &a.key, 1 + 10_000_000).unwrap());
    assert_eq!(next(&s, 2).unwrap().seq, 2, "numbered from the local head");
    assert!(memory::saw(&s, &a.key, 2 + 5).unwrap());
    assert_eq!(next(&s, 3).unwrap().seq, 8, "a plausible claim is honoured");
    // A value already past the range (an older build stored it) refuses, never panics.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("UPDATE memory_seen SET seq = ?1", [i64::MAX])
        .unwrap();
    assert!(next(&s, 4).is_err());
}

/// A state.db from before stemming gets its index rebuilt stemmed, entries kept.
#[test]
fn an_older_index_is_rebuilt_stemmed() {
    let a = identity();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let s = Store::open(&path).unwrap();
    memory::remember(&s, &a, rem("test runner", "cargo test", &[]), 1).unwrap();
    drop(s);
    // Back to v11: the index as V9 made it, no `memory_seen`.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(
            "DROP TABLE memory_fts; DROP TABLE memory_seen;
             CREATE VIRTUAL TABLE memory_fts USING fts5(
                 subject, fact, content='memory_entries', content_rowid='rowid');
             INSERT INTO memory_fts(rowid, subject, fact) SELECT rowid,
                 json_extract(entry, '$.op.subject'), json_extract(entry, '$.op.fact')
                 FROM memory_entries;
             PRAGMA user_version = 11;",
        )
        .unwrap();
    let s = Store::open(&path).unwrap();
    assert_eq!(memory::search(&s, "tests", 5).unwrap().len(), 1);
    // The triggers still feed the new index.
    memory::remember(&s, &a, rem("deploy", "fridays", &[]), 2).unwrap();
    assert_eq!(memory::search(&s, "deploys", 5).unwrap().len(), 1);
}

/// A question of stopwords only is matched as the whole phrase.
#[test]
fn a_stopword_question_matches_as_a_phrase() {
    let a = identity();
    let (_d, s) = team(&a);
    memory::remember(&s, &a, rem("motto", "what we do is ship", &[]), 1).unwrap();
    memory::remember(&s, &a, rem("ci", "we use what works", &[]), 2).unwrap();
    let hits = memory::search(&s, "what we", 5).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].subject, "motto");
    assert!(memory::search(&s, "how is it", 5).unwrap().is_empty());
}

#[test]
fn refuse_secret_matches_the_six_patterns() {
    for (text, name) in [
        ("password = hunter2hunter2", "credential"),
        ("-----BEGIN RSA PRIVATE KEY-----", "private_key"),
        ("AKIAABCDEFGHIJKLMNOP", "aws_key"),
        ("sk-abcdefghijklmnopqrstuv", "api_key"),
        ("https://bob:pw123@example.com/x", "url_credentials"),
        ("eyJhbGciOiJIUzI1.eyJzdWIiOiIxMjM0NTY", "jwt"),
    ] {
        assert_eq!(memory::refuse_secret(text), Some(name), "{text}");
    }
    assert_eq!(
        memory::refuse_secret("we use postgres, token bucket limiter"),
        None
    );
}

#[test]
fn entry_over_16kb_is_refused() {
    let a = identity();
    let (_d, s) = team(&a);
    let err = memory::remember(&s, &a, rem("big", &"x".repeat(16 * 1024), &[]), 1).unwrap_err();
    assert_eq!(err.to_string(), "too_long");
    assert!(memory::remember(&s, &a, rem("ok", &"x".repeat(8 * 1024), &[]), 1).is_ok());
}

#[test]
fn remember_refuses_secret_with_reason() {
    let a = identity();
    let (_d, s) = team(&a);
    let err = memory::remember(
        &s,
        &a,
        rem("deploy", "-----BEGIN RSA PRIVATE KEY-----", &[]),
        1,
    )
    .unwrap_err();
    assert!(err.to_string().contains("private_key"), "{err}");
    let err = memory::remember(&s, &a, rem("AKIAABCDEFGHIJKLMNOP", "x", &[]), 1).unwrap_err();
    assert!(err.to_string().contains("aws_key"), "{err}");
    assert!(memory::heads(&s).unwrap().is_empty());
    assert!(memory::facts(&s, None, None).unwrap().is_empty());
}

/// `who` admitted to `s`'s roster by founder `a`.
fn admit(s: &Store, a: &Identity, who: &Identity) {
    let g = roster::entries(s).unwrap().remove(0);
    let adm = roster::Entry::signed(
        a,
        vec![g.hash()],
        RosterKind::Admit,
        json!({"member": who.key, "name": "b", "computer": "mac", "role": "member"}),
        2,
    );
    roster::accept(s, &adm, 10).unwrap();
}

/// `b`'s first entry: a forget of `target`, made on a store of its own.
fn forget_by(b: &Identity, target: &str) -> memory::Entry {
    let (_d, sb) = store();
    memory::remember(
        &sb,
        b,
        Op::Forget {
            target: target.into(),
        },
        2,
    )
    .unwrap()
}

#[test]
fn non_author_forget_before_body_wins() {
    let (a, b) = (identity(), identity());
    let (_d1, s1) = team(&a);
    let x = memory::remember(&s1, &a, rem("db", "postgres", &[]), 1).unwrap();
    let fa = memory::remember(
        &s1,
        &a,
        Op::Forget {
            target: x.id.clone(),
        },
        2,
    )
    .unwrap();
    let fb = forget_by(&b, &x.id);

    let (_d2, s2) = team(&a);
    admit(&s2, &a, &b);
    assert!(memory::accept(&s2, &fb, 3).unwrap());
    assert!(memory::accept(&s2, &fa, 3).unwrap());
    assert!(memory::accept(&s2, &x, 3).unwrap());
    assert!(memory::facts(&s2, None, None).unwrap().is_empty());
    assert!(memory::search(&s2, "postgres", 5).unwrap().is_empty());
    let h = memory::heads(&s2).unwrap();
    assert_eq!((h[&a.key], h[&b.key]), (2, 1));
    assert_eq!(memory::since(&s2, &a.key, 0, 10).unwrap(), vec![fa]);
    assert_eq!(memory::since(&s2, &b.key, 0, 10).unwrap(), vec![fb]);
}

#[test]
fn forget_of_known_entry_by_other_member_applies() {
    let (a, b) = (identity(), identity());
    let (_d, s) = team(&a);
    admit(&s, &a, &b);
    let x = memory::remember(&s, &a, rem("db", "postgres", &[]), 1).unwrap();
    assert_eq!(memory::facts(&s, None, None).unwrap().len(), 1);
    let (_db, sb) = store();
    let fb = memory::remember(
        &sb,
        &b,
        Op::Forget {
            target: x.id.clone(),
        },
        2,
    )
    .unwrap();
    // A forget of a forget is accepted and changes nothing.
    let ff = memory::remember(
        &sb,
        &b,
        Op::Forget {
            target: fb.id.clone(),
        },
        3,
    )
    .unwrap();
    assert!(memory::accept(&s, &fb, 3).unwrap());
    assert!(memory::accept(&s, &ff, 3).unwrap());
    assert!(memory::facts(&s, None, None).unwrap().is_empty());
    assert_eq!(memory::since(&s, &b.key, 0, 10).unwrap(), vec![fb, ff]);
}

#[test]
fn search_survives_snapshot() {
    let a = identity();
    let (d, s) = team(&a);
    memory::remember(&s, &a, rem("gone", "temp", &[]), 1).unwrap();
    memory::remember(&s, &a, rem("database choice", "postgres", &[]), 3).unwrap();
    memory::remember(&s, &a, rem("ci provider", "actions", &[]), 4).unwrap();
    // A hard delete leaves a gap in the rowids: VACUUM would renumber a table without an
    // explicit rowid, and the index would point at the wrong rows.
    rusqlite::Connection::open(d.path().join("state.db"))
        .unwrap()
        .execute("DELETE FROM memory_entries WHERE seq = 1", [])
        .unwrap();
    let copy = d.path().join("copy.db");
    s.snapshot(&copy).unwrap();
    let c = Store::open(&copy).unwrap();
    assert_eq!(
        memory::search(&c, "database", 5).unwrap()[0].value,
        "postgres"
    );
    assert_eq!(
        memory::search(&c, "provider", 5).unwrap()[0].value,
        "actions"
    );
}
