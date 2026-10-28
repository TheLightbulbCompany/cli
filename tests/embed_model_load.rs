//! A model that is installed but does not load is tried once per process. Its own test binary:
//! the load (or its failure) is kept for the process, so it must not share one with the tests
//! that load the real model.
use lightbulb::{embed_model, memory, paths};

#[test]
fn a_model_that_fails_to_load_is_tried_once() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var(paths::HOME_ENV, dir.path());
    let model = dir.path().join(paths::MODEL);
    std::fs::create_dir_all(&model).unwrap();
    for f in ["config.json", "tokenizer.json", "model.safetensors"] {
        std::fs::write(model.join(f), "corrupt").unwrap();
    }
    assert!(embed_model::installed());
    let store = lightbulb::store::Store::open(&dir.path().join("state.db")).unwrap();
    // Two recalls: keyword-only, and one load attempt between them.
    for _ in 0..2 {
        assert!(memory::search(&store, "deploy keys", 5).unwrap().is_empty());
    }
    assert_eq!(embed_model::text("anything"), None);
    assert_eq!(embed_model::load_attempts(), 1);
}
