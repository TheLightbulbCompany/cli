//! The local embedding model (`embed_model`). The tests that need the real model download it
//! themselves and run only with `LIGHTBULB_TEST_MODEL=1` (CI sets it).
mod common;

use lightbulb::{capture, embed_model, memory, paths};
use std::sync::Mutex;
use std::time::Instant;

/// The state dir is a process-wide env var: one test at a time points it somewhere.
static STATE: Mutex<()> = Mutex::new(());

fn wants_model() -> bool {
    std::env::var("LIGHTBULB_TEST_MODEL").as_deref() == Ok("1")
}

#[test]
fn without_a_model_text_is_none_and_installed_false() {
    let _g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var(paths::HOME_ENV, dir.path());
    assert!(!embed_model::installed());
    assert_eq!(embed_model::text("anything at all"), None);
}

#[test]
#[ignore = "downloads the model; run with LIGHTBULB_TEST_MODEL=1 and --include-ignored"]
fn two_paraphrases_are_closer_than_two_unrelated_texts() {
    if !wants_model() {
        return;
    }
    let _g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var(paths::HOME_ENV, dir.path());
    // A partial model/ from an earlier, broken run does not block the install.
    std::fs::create_dir_all(dir.path().join("model")).unwrap();
    std::fs::write(dir.path().join("model/tokenizer.json"), "partial").unwrap();
    embed_model::install(dir.path()).unwrap();
    embed_model::install(dir.path()).unwrap(); // idempotent
    assert!(embed_model::installed());
    let v = |s: &str| embed_model::text(s).unwrap();
    let a = v("The build failed because the database migration timed out.");
    let b = v("CI broke since the DB schema migration took too long.");
    let c = v("My cat likes to sleep in the afternoon sun by the window.");
    assert_eq!(a.len(), embed_model::DIM);
    let (near, far) = (embed_model::cosine(&a, &b), embed_model::cosine(&a, &c));
    assert!(
        near > far + 0.05,
        "paraphrase {near} not clearly above unrelated {far}"
    );
    // Edge inputs give a vector and do not panic.
    assert_eq!(v("").len(), embed_model::DIM);
    assert_eq!(v(&"x".repeat(100_000)).len(), embed_model::DIM);
    // ≤ 20 ms per 200-character entry is the target; 100 ms is the bound on a slow runner.
    let entry = "The deploy reads staging credentials from the vault. ".repeat(4);
    let entry = &entry[..200];
    let t = Instant::now();
    v(entry);
    assert!(t.elapsed().as_millis() < 100, "took {:?}", t.elapsed());
}

#[test]
fn cosine_of_identical_vectors_is_one_and_orthogonal_is_zero() {
    let a = [0.3, -1.2, 2.0];
    assert!((embed_model::cosine(&a, &a) - 1.0).abs() < 1e-6);
    assert_eq!(embed_model::cosine(&[1.0, 0.0], &[0.0, 2.0]), 0.0);
}

/// The daemon's housekeeping fills the vectors missing when it starts: an entry and a turn
/// stored before the model was installed get one; a refused turn never does; an entry body this
/// build cannot read is skipped once and does not stop the rows after it.
#[test]
#[ignore = "downloads the model; run with LIGHTBULB_TEST_MODEL=1 and --include-ignored"]
fn the_daemon_embeds_what_has_no_vector_at_start() {
    if !wants_model() {
        return;
    }
    let _g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let e = common::Env::new();
    std::env::set_var(paths::HOME_ENV, e.state.path());
    // No model yet: nothing is embedded as it is stored.
    let store = e.store();
    let op = memory::Op::Remember {
        subject: "db".into(),
        fact: "we run postgres 16".into(),
        kind: memory::Kind::Fact,
        scope: memory::Scope::Team,
        when: None,
        source: memory::Source {
            harness: "test".into(),
            repo: None,
            session: None,
            on_behalf_of: None,
            shared_from: None,
        },
        replaces: vec![],
    };
    // An entry this build cannot read (JSON, but not an entry), before the good one.
    drop(store);
    rusqlite::Connection::open(e.state.path().join("state.db"))
        .unwrap()
        .execute(
            "INSERT INTO memory_entries (id, author, seq, entry, received_at)
             VALUES ('bad', 'x', 1, '{\"op\": {\"t\": \"remember\"}}', 1)",
            [],
        )
        .unwrap();
    let store = e.store();
    memory::remember(&store, &e.identity(), op, 1).unwrap();
    let row = capture::SessionRow {
        id: "a:1".into(),
        harness: "a".into(),
        computer: "mac".into(),
        repo: None,
        branch: None,
        started: 1,
        ended: None,
        prompt_tokens: None,
        completion_tokens: None,
    };
    capture::start_session(&store, &row, "/tmp/proj").unwrap();
    // Today: the daemon prunes turns older than 90 days as it starts.
    let now = common::now_s();
    // The refused turn first: a pass that reaches the clean turn has passed it.
    capture::record_turn(
        &store,
        "a:1",
        "assistant",
        "export API_KEY=abcd1234efgh5678",
        now,
    )
    .unwrap();
    capture::record_turn(&store, "a:1", "user", "list the files", now).unwrap();
    drop(store);
    let db = e.state.path().join("state.db");
    let vecs = |sql: &str| common::count(&db, sql);
    let entry = "SELECT COUNT(*) FROM memory_vec WHERE length(v) > 0";
    let skipped = "SELECT COUNT(*) FROM memory_vec WHERE id = 'bad' AND length(v) = 0";
    let turn = |seq: i64| {
        format!(
            "SELECT COUNT(*) FROM turns_vec JOIN capture_turns ON turn = capture_turns.rowid \
             WHERE seq = {seq}"
        )
    };
    assert_eq!((vecs(entry), vecs(&turn(2))), (0, 0));
    embed_model::install(e.state.path()).unwrap();
    e.daemon(false, "", &[]);
    common::until(30, "the start-up pass embeds", || {
        vecs(entry) == 1 && vecs(&turn(2)) == 1
    });
    assert_eq!(vecs(&turn(1)), 0, "a refused turn is never embedded");
    assert_eq!(
        vecs(skipped),
        1,
        "the unreadable entry is marked, never picked again"
    );
}
