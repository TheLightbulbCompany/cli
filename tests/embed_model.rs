//! The local embedding model (`embed_model`). The tests that need the real model download it
//! themselves and run only with `LIGHTBULB_TEST_MODEL=1` (CI sets it).
use lightbulb::{embed_model, paths};
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
