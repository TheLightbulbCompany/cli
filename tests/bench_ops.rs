//! The benchmark-only daemon ops (`--features bench`): dated turns, and recall as of a moment.
#![cfg(feature = "bench")]

use serde_json::{json, Value};

mod common;

fn daemon() -> common::Env {
    let e = common::Env::new();
    e.daemon(false, "", &[]);
    e
}

fn ingest(e: &common::Env, session: &str, turns: Value) -> Value {
    e.ask(json!({"id": 1, "op": "bench.ingest_turns", "session": session, "turns": turns}))
}

fn remember(e: &common::Env, subject: &str, fact: &str, when: i64, replaces: Value) -> Value {
    e.ask(
        json!({"id": 2, "op": "memory.remember", "subject": subject, "fact": fact,
        "kind": "fact", "when": when, "replaces": replaces}),
    )
}

fn recall(e: &common::Env, question: &str, as_of: Option<i64>) -> Value {
    e.ask(
        json!({"id": 3, "op": "memory.recall", "question": question, "as_of": as_of,
        "turn_limit": 50}),
    )
}

#[test]
fn ingested_turns_keep_their_order_and_given_times() {
    let e = daemon();
    let r = ingest(
        &e,
        "bench:c1",
        json!([
            {"role": "user", "text": "the database is postgres", "at": 1_000},
            {"role": "assistant", "text": "noted, the database is postgres", "at": 1_001},
        ]),
    );
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["stored"], 2);
    assert_eq!(e.ask(json!({"id": 4, "op": "bench.settle"}))["ok"], true);
    let t = recall(&e, "database postgres", None)["turns"].clone();
    let got: Vec<(i64, i64)> = t
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h["seq"].as_i64().unwrap(), h["at"].as_i64().unwrap()))
        .collect();
    assert_eq!(got.len(), 2, "{t}");
    assert!(
        got.contains(&(1, 1_000)) && got.contains(&(2, 1_001)),
        "{t}"
    );
}

#[test]
fn recall_as_of_sees_only_what_was_known_then() {
    let e = daemon();
    let first = remember(&e, "database", "postgres", 1_000, json!([]));
    remember(&e, "database", "mysql", 3_000, json!([first["entry_id"]]));
    ingest(
        &e,
        "bench:c1",
        json!([
            {"role": "user", "text": "we picked the database postgres", "at": 1_000},
            {"role": "user", "text": "we moved the database to mysql", "at": 3_000},
        ]),
    );

    let then = recall(&e, "database", Some(2_000));
    assert_eq!(then["as_of"], 2_000);
    let facts: Vec<(&str, &str)> = then["facts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["value"].as_str().unwrap(), f["status"].as_str().unwrap()))
        .collect();
    assert_eq!(facts, vec![("postgres", "current")], "{then}");
    let turns = then["turns"].as_array().unwrap();
    assert!(
        turns.iter().all(|t| t["at"].as_i64().unwrap() <= 2_000) && !turns.is_empty(),
        "{then}"
    );

    let now = recall(&e, "database", None);
    assert!(now.get("as_of").is_none() || now["as_of"].is_null());
    let values: Vec<&str> = now["facts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["value"].as_str().unwrap())
        .collect();
    assert_eq!(values, vec!["mysql"], "{now}");
}
