//! `memory.remember` and `memory.recall` through a real daemon's socket.

use serde_json::{json, Value};

mod common;

fn remember(e: &common::Env, subject: &str, fact: &str, kind: &str, extra: Value) -> Value {
    let mut body =
        json!({"id": 1, "op": "memory.remember", "subject": subject, "fact": fact, "kind": kind});
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    e.ask(body)
}

fn recall(e: &common::Env, question: &str, budget: Option<u64>) -> Value {
    e.ask(json!({"id": 2, "op": "memory.recall", "question": question, "budget_chars": budget}))
}

fn daemon() -> common::Env {
    let e = common::Env::new();
    e.daemon(false, "", &[]);
    e
}

#[test]
fn test_remember_conflict_without_replaces_is_contested() {
    let e = daemon();
    let first = remember(
        &e,
        "launch date",
        "3 March",
        "fact",
        json!({"when": 1_700_000_000}),
    );
    assert_eq!(first["ok"], true);
    assert_eq!(first["stored"], true);
    assert_eq!(first["neighbours"], json!([]));
    let second = remember(
        &e,
        "launch date",
        "10 March",
        "fact",
        json!({"when": 1_700_100_000}),
    );
    assert_eq!(second["stored"], true);
    let n = second["neighbours"].as_array().unwrap();
    assert_eq!(n.len(), 1);
    assert_eq!(n[0]["value"], "3 March");
    assert_eq!(n[0]["id"], first["entry_id"]);

    let r = recall(&e, "launch date", None);
    let page = r["page"].as_str().unwrap();
    // One author's two values are that author's notes over time: two dated lines, not a
    // "Contested" one (the facts stay contested in the JSON).
    let day = |s: i64| lightbulb::date::utc(s)[..10].to_string();
    assert!(page.starts_with("Current:\n"), "{page}");
    assert!(
        !page.contains("Contested:") && !page.contains(" vs "),
        "{page}"
    );
    assert!(
        page.contains(&format!(
            "launch date: 3 March (prasiddha, {})",
            day(1_700_000_000)
        )),
        "{page}"
    );
    assert!(
        page.contains(&format!(
            "launch date: 10 March (prasiddha, {})",
            day(1_700_100_000)
        )),
        "{page}"
    );
    assert!(r["facts"]
        .as_array()
        .unwrap()
        .iter()
        .all(|f| f["status"] == "contested"));

    // A second call that replaces the first settles it, with the old value as history.
    let third = remember(
        &e,
        "launch date",
        "10 March",
        "fact",
        json!({"when": 1_700_200_000, "replaces": [first["entry_id"], second["entry_id"]]}),
    );
    assert_eq!(third["stored"], true);
    let page = recall(&e, "launch date", None)["page"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        page.starts_with("Current:\nlaunch date: 10 March ("),
        "{page}"
    );
    assert!(page.contains("; was 10 March, "), "{page}");
}

#[test]
fn remember_refuses_secret_with_reason() {
    let e = daemon();
    let r = remember(
        &e,
        "deploy",
        "key -----BEGIN RSA PRIVATE KEY----- abc",
        "fact",
        json!({}),
    );
    assert_eq!(r["ok"], true);
    assert_eq!(r["stored"], false);
    assert_eq!(r["refused"], "secret:private_key");
    assert_eq!(recall(&e, "", None)["facts"], json!([]));
    let r = remember(&e, "notes", &"x".repeat(20_000), "fact", json!({}));
    assert_eq!(r["refused"], "too_long");
    assert_eq!(r["stored"], false);
    // The text never reaches the daemon log.
    let log = std::fs::read_to_string(e.state.path().join("daemon.log")).unwrap();
    assert!(!log.contains("PRIVATE KEY"));
}

#[test]
fn recall_page_puts_contested_first_and_respects_budget() {
    let e = daemon();
    remember(&e, "alpha", "one", "fact", json!({}));
    remember(&e, "beta", "two", "lesson", json!({"scope": "team"}));
    for (v, when) in [("red", 1_700_000_000), ("blue", 1_700_100_000)] {
        remember(&e, "colour", v, "preference", json!({"when": when}));
    }
    let page = recall(&e, "", None)["page"].as_str().unwrap().to_string();
    // One person's two colours are two dated lines; Contested needs two people
    // (memory_store: the_page_says_contested_only_when_two_people_disagree).
    let (c, cur, l) = (
        page.find("Contested:"),
        page.find("Current:"),
        page.find("Lessons:"),
    );
    assert!(c.is_none() && cur == Some(0) && cur < l, "{page}");
    assert!(
        page.contains("colour: blue (") && page.contains("colour: red ("),
        "{page}"
    );
    assert!(
        page.contains("alpha: one (") && page.contains("beta: two ("),
        "{page}"
    );

    // Cut on a line boundary: room for the first line only.
    let first_line = page.lines().nth(1).unwrap();
    let budget = "Current:\n".len() + first_line.len() + 1;
    let cut = recall(&e, "", Some(budget as u64))["page"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(cut, format!("Current:\n{first_line}"));
    assert_eq!(recall(&e, "", Some(3))["page"], "");
}

/// Starts session `claude:c1` in a folder and records `turns` (role, text) on it.
fn turns(e: &common::Env, turns: &[(&str, &str)]) {
    let start = json!({"id": 3, "op": "capture.session_start", "harness": "claude",
        "conversation": "c1", "cwd": "/tmp/proj"});
    assert_eq!(e.ask(start)["stored"], true);
    for (role, text) in turns {
        let r = e.ask(json!({"id": 4, "op": "capture.turn", "harness": "claude",
            "conversation": "c1", "role": role, "text": text}));
        assert_eq!(r["stored"], true, "{r}");
    }
}

#[test]
fn recall_returns_an_excerpt_from_a_turn() {
    let e = daemon();
    let long = format!(
        "rotate the staging deploy keys \r\n  with the vault script {}",
        "é".repeat(1000)
    );
    turns(&e, &[("user", "nothing to see here"), ("assistant", &long)]);
    let r = recall(&e, "rotate deploy keys", None);
    let t = r["turns"].as_array().unwrap();
    assert_eq!(t.len(), 1, "{r}");
    assert_eq!(t[0]["session"], "claude:c1");
    assert_eq!(t[0]["seq"], 2);
    assert_eq!(t[0]["role"], "assistant");
    let excerpt = t[0]["excerpt"].as_str().unwrap();
    assert!(excerpt.starts_with("rotate the staging deploy keys with the vault"));
    // Cut at `EXCERPT_MAX` characters, on a character boundary (é is two bytes).
    assert_eq!(excerpt.chars().count(), lightbulb::capture::EXCERPT_MAX);
    let day = &lightbulb::date::utc(t[0]["at"].as_i64().unwrap())[..10];
    let page = r["page"].as_str().unwrap();
    assert!(
        page.contains(&format!("Turns:\nassistant, {day}: {excerpt}")),
        "{page}"
    );
}

/// `sessions turns off` also hides the turns already stored from recall; they stay for
/// `sessions show` and `sessions forget`, and come back when turns are on again.
#[test]
fn turns_off_hides_stored_turns_from_recall() {
    let e = daemon();
    turns(&e, &[("user", "rotate the staging deploy keys")]);
    assert_eq!(
        recall(&e, "deploy keys", None)["turns"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    lightbulb::config::Config::set(e.state.path(), "capture_turns", "false").unwrap();
    let r = recall(&e, "deploy keys", None);
    assert_eq!(r["turns"], json!([]), "{r}");
    assert!(!r["page"].as_str().unwrap().contains("rotate"), "{r}");
    assert_eq!(
        lightbulb::capture::turns(&e.store(), "claude:c1")
            .unwrap()
            .len(),
        1
    );
    lightbulb::config::Config::set(e.state.path(), "capture_turns", "true").unwrap();
    assert_eq!(
        recall(&e, "deploy keys", None)["turns"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn the_start_block_has_no_turns() {
    let e = daemon();
    remember(&e, "deploy", "the vault script", "fact", json!({}));
    turns(&e, &[("user", "the deploy uses the vault script")]);
    let r = e.ask(json!({"id": 2, "op": "memory.recall", "question": "",
        "repo": "github.com/acme/proj", "session": "claude:c1"}));
    assert_eq!(r["turns"], json!([]), "{r}");
    assert!(!r["block"].as_str().unwrap().contains("Turns:"), "{r}");
    // Without a repo too: an empty question lists the facts, never turns.
    let r = recall(&e, "", None);
    assert_eq!(r["turns"], json!([]), "{r}");
    let page = r["page"].as_str().unwrap();
    assert!(
        page.contains("deploy: the vault script") && !page.contains("Turns:"),
        "{page}"
    );
}

#[test]
fn a_refused_turn_is_never_returned() {
    let e = daemon();
    turns(&e, &[("user", "export API_KEY=abcd1234efgh5678")]);
    let r = recall(&e, "refused credential", None);
    assert_eq!(r["turns"], json!([]), "{r}");
    assert!(!r["page"].as_str().unwrap().contains("refused"), "{r}");
}

#[test]
fn turns_from_another_repo_are_not_recalled() {
    let e = daemon();
    for (c, repo) in [("c1", "acme/one"), ("c2", "acme/two")] {
        let start = json!({"id": 3, "op": "capture.session_start", "harness": "claude",
            "conversation": c, "cwd": format!("/tmp/{c}"), "repo": repo});
        assert_eq!(e.ask(start)["stored"], true);
        let r = e.ask(json!({"id": 4, "op": "capture.turn", "harness": "claude",
            "conversation": c, "role": "user", "text": format!("deploy {repo} today")}));
        assert_eq!(r["stored"], true, "{r}");
    }
    let r =
        e.ask(json!({"id": 2, "op": "memory.recall", "question": "deploy", "repo": "acme/one"}));
    let t = r["turns"].as_array().unwrap();
    assert_eq!(t.len(), 1, "{r}");
    assert_eq!(t[0]["session"], "claude:c1");
    assert!(!r["page"].as_str().unwrap().contains("acme/two"), "{r}");
    // A caller with no repo searches every turn on this computer.
    assert_eq!(
        recall(&e, "deploy", None)["turns"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

/// A question's page holds every matching turn up to 50, not 5; turns that rank the same come
/// newest first.
#[test]
fn recall_holds_more_than_five_turns_newest_first_on_a_tie() {
    let e = daemon();
    let texts: Vec<String> = (10..22)
        .map(|i| format!("rotate the deploy keys, note {i}"))
        .collect();
    let pairs: Vec<(&str, &str)> = texts.iter().map(|t| ("user", t.as_str())).collect();
    turns(&e, &pairs);
    let r = recall(&e, "rotate deploy keys", None);
    let seqs: Vec<i64> = r["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["seq"].as_i64().unwrap())
        .collect();
    assert_eq!(seqs, (1..=12).rev().collect::<Vec<i64>>(), "{r}");
    assert_eq!(r["page"].as_str().unwrap().matches("user, ").count(), 12);
}

/// The excerpt is the 800-character window that holds the question's words, not the turn's
/// opening.
#[test]
fn an_excerpt_is_the_window_that_matches() {
    let e = daemon();
    let filler = "lorem ipsum dolor sit amet ".repeat(60);
    let text = format!("{filler}reset the counters every minute and queue the requests {filler}");
    turns(&e, &[("assistant", &text)]);
    let r = recall(&e, "how should we queue requests and reset counters?", None);
    let x = r["turns"][0]["excerpt"].as_str().unwrap();
    assert!(x.starts_with('…'), "{x}");
    assert!(
        x.contains("reset the counters every minute and queue the requests"),
        "{x}"
    );
    assert!(
        x.chars().count() <= lightbulb::capture::EXCERPT_MAX,
        "{}",
        x.chars().count()
    );
}

/// The evidence stops at 8,000 tokens counted as the benchmark counts them (characters / 4,
/// rounded up, of `subject: value` and `role: excerpt`): the first result that would overflow
/// ends the page, and every result kept is a line on it (the trim is the only cutter).
#[test]
fn recall_trims_its_evidence_at_8000_tokens() {
    let e = daemon();
    for i in 0..5 {
        let r = remember(
            &e,
            &format!("rotate note {i}"),
            "deploy keys go through the vault",
            "fact",
            json!({}),
        );
        assert_eq!(r["stored"], true, "{r}");
    }
    let texts: Vec<String> = (100..150)
        .map(|i| format!("rotate the deploy keys {i} {}", "lorem ".repeat(200)))
        .collect();
    let pairs: Vec<(&str, &str)> = texts.iter().map(|t| ("assistant", t.as_str())).collect();
    turns(&e, &pairs);
    let r = recall(&e, "rotate deploy keys", None);
    let tokens: Vec<usize> = r["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            format!("assistant: {}", t["excerpt"].as_str().unwrap())
                .chars()
                .count()
                .div_ceil(4)
        })
        .collect();
    // Each is 800 characters: (11 + 800) / 4 rounded up is 203 tokens, so 39 fit, not 50.
    assert!(tokens.iter().all(|&n| n == 203), "{tokens:?}");
    assert_eq!(tokens.len(), 39);
    assert!(tokens.iter().sum::<usize>() <= lightbulb::memory::RECALL_TOKENS);
    // The counts are the page's lines, author and date included.
    let page = r["page"].as_str().unwrap();
    let lines = |prefix: &str| page.lines().filter(|l| l.starts_with(prefix)).count();
    assert_eq!(r["facts"].as_array().unwrap().len(), 5, "{r}");
    assert_eq!(lines("rotate note "), 5, "{page}");
    assert_eq!(lines("assistant, "), 39, "{page}");
}

/// The session-start block, asked as the hook asks it (hook.rs: no question, the session's repo,
/// 1,600 characters, its key), is what it was: facts only, within 1,600 characters, never trimmed
/// to the question page's 100 results, while a question's page runs past it.
#[test]
fn the_start_block_keeps_its_1600_characters() {
    let e = daemon();
    let me = lightbulb::os::process(std::process::id()).unwrap();
    let r = e.ask(
        json!({"id": 1, "op": "register", "session": {"provider": "claude",
        "conversation": "s1", "pid": me.pid, "birth_micros": me.birth_micros, "cwd": "/tmp",
        "repo": "engine"}}),
    );
    let key = r["key"].as_str().unwrap().to_string();
    // 120 facts in this repo: past the question page's 100 results.
    for i in 0..120 {
        let r = remember(
            &e,
            &format!("deploy step {i}"),
            &format!("the deploy runs the vault script with the staging profile, step {i}"),
            "fact",
            json!({ "session": key }),
        );
        assert_eq!(r["stored"], true, "{r}");
    }
    // A turn in the same repo, which the start block never shows.
    let start = json!({"id": 3, "op": "capture.session_start", "harness": "claude",
        "conversation": "s1", "cwd": "/tmp/proj", "repo": "engine"});
    assert_eq!(e.ask(start)["stored"], true);
    let turn = json!({"id": 4, "op": "capture.turn", "harness": "claude", "conversation": "s1",
        "role": "user", "text": "the deploy uses the vault script"});
    assert_eq!(e.ask(turn)["stored"], true);
    let start = e.ask(
        json!({"id": 5, "op": "memory.recall", "question": "", "repo": "engine",
        "budget_chars": 1600, "session": key}),
    );
    let page = start["page"].as_str().unwrap();
    assert!(
        page.chars().count() <= 1600 && !page.contains("Turns:"),
        "{page}"
    );
    assert!(start["block"]
        .as_str()
        .unwrap()
        .starts_with("Memory (team and yours):"));
    assert_eq!(start["facts"].as_array().unwrap().len(), 120, "not trimmed");
    assert_eq!(start["turns"], json!([]));
    let asked = recall(&e, "deploy vault script", None);
    assert!(
        asked["page"].as_str().unwrap().chars().count() > 1600,
        "{asked}"
    );
}
