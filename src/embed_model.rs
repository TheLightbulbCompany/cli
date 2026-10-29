//! A small local embedding model behind `text`: `minishlab/potion-base-8M`, a static
//! (Model2Vec) model run by `model2vec-rs`. `setup` downloads it once into `<state dir>/model/`;
//! nothing else leaves the computer. Without it, `text` is `None` and memory is keyword-only.
//!
//! Why this one (measured 2026-10-28 on an M-series Mac, release builds, a 240-character text):
//! - `model2vec-rs` 0.3 + potion-base-8M: download 30.9 MB, 0.08 ms per text (load 26 ms),
//!   dim 256; the `lightbulb` release binary grows 25.3 → 27.7 MB (+2.3 MB) with `text`
//!   reachable. Pure Rust: tokenizers with `fancy-regex` only (no onig, no C++ suffix array),
//!   so no system library and no new C build.
//! - `fastembed` 7.5 + all-MiniLM-L6-v2 int8 ONNX: download 23.7 MB, 12 ms per text (load
//!   0.7 s), but the binary grows by 29 MB (> the 25 MB bound) and links a prebuilt ONNX
//!   Runtime fetched at build time (libc++, CoreML on macOS). Rejected on size and native deps.
use crate::{paths, Result};
use model2vec_rs::model::StaticModel;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The model's dimension (potion-base-8M).
pub const DIM: usize = 256;

/// The files, pinned to one revision of the model and checked by SHA-256.
const BASE: &str =
    "https://huggingface.co/minishlab/potion-base-8M/resolve/bf8b056651a2c21b8d2565580b8569da283cab23";
const FILES: [(&str, &str); 3] = [
    (
        "config.json",
        "2a6ac0e9aaa356a68a5688070db78fc3a464fefe85d2f06a1905ce3718687553",
    ),
    (
        "tokenizer.json",
        "e67e803f624fb4d67dea1c730d06e1067e1b14d830e2c2202569e3ef0f70bb50",
    ),
    (
        "model.safetensors",
        "f65d0f325faadc1e121c319e2faa41170d3fa07d8c89abd48ca5358d9a223de2",
    ),
];
/// (tests) Where to download from instead of `BASE`.
const URL_ENV: &str = "LIGHTBULB_CONNECT_DEBUG_MODEL_URL";

fn model_dir(state: &Path) -> PathBuf {
    state.join(paths::MODEL)
}

/// Whether the model is in this computer's state dir (`doctor`, `setup`).
pub fn installed() -> bool {
    installed_in(&paths::state_path())
}

/// Whether the model is in the state dir `dir` (a store's: `Store::dir`).
pub fn installed_in(dir: &Path) -> bool {
    model_dir(dir).join(FILES[2].0).is_file()
}

/// How many times this process has tried to load the model (`text`): at most once.
static LOADS: AtomicUsize = AtomicUsize::new(0);

/// (tests) How many times this process has tried to load the model.
#[doc(hidden)]
pub fn load_attempts() -> usize {
    LOADS.load(Ordering::SeqCst)
}

/// `text_in` this computer's state dir.
pub fn text(s: &str) -> Option<Vec<f32>> {
    text_in(&paths::state_path(), s)
}

/// `s` embedded (`DIM` floats, unit length) by the model in state dir `dir`; `None` when none
/// is installed there, or when the installed one did not load (remembered: no call tries
/// again, none under the store lock).
/// ponytail: one model per process, loaded from the first dir asked (the daemon has one state
/// dir; tests that load the real model have their own binary); the first load, or its failure,
/// is kept, and a restart of the service tries again.
pub fn text_in(dir: &Path, s: &str) -> Option<Vec<f32>> {
    static MODEL: OnceLock<Option<StaticModel>> = OnceLock::new();
    if !installed_in(dir) {
        return None;
    }
    let model = MODEL.get_or_init(|| {
        LOADS.fetch_add(1, Ordering::SeqCst);
        StaticModel::from_pretrained(model_dir(dir), None, None, None).ok()
    });
    Some(model.as_ref()?.encode_single(s))
}

/// Downloads the model into `<dir>/model/` unless it is there. Written to a temp dir and
/// renamed, so a failed download leaves nothing behind.
pub fn install(dir: &Path) -> Result<()> {
    let dest = model_dir(dir);
    let done = || dest.join(FILES[2].0).is_file();
    if done() {
        return Ok(());
    }
    let base = crate::cli::test_env(URL_ENV)
        .map(|u| u.to_string_lossy().into_owned())
        .unwrap_or_else(|| BASE.into());
    let tmp = dir.join(format!("{}.tmp-{}", paths::MODEL, std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    // Each file gets 120 s and the whole install 300 s, so a slow network cannot hold setup up.
    let deadline = Instant::now() + Duration::from_secs(300);
    let fetched = FILES.iter().try_for_each(|(f, sum)| -> Result<()> {
        let left = deadline.saturating_duration_since(Instant::now()).as_secs();
        if left == 0 {
            return Err("the download took too long.".into());
        }
        let bytes = crate::cli::curl_for(&format!("{base}/{f}"), false, left.min(120))?;
        if format!("{:x}", Sha256::digest(&bytes)) != *sum {
            return Err(format!("{f} did not match its checksum").into());
        }
        Ok(fs::write(tmp.join(f), bytes)?)
    });
    let result = fetched.and_then(|()| {
        // A partial model/ left by an older run would block the rename.
        if !done() {
            let _ = fs::remove_dir_all(&dest);
        }
        Ok(fs::rename(&tmp, &dest)?)
    });
    let _ = fs::remove_dir_all(&tmp);
    // Another setup may have renamed its copy in first.
    result.or_else(|e| if done() { Ok(()) } else { Err(e) })
}

/// The model's size on disk in MB (0 when it is not installed).
pub fn size_mb() -> u64 {
    let dir = model_dir(&paths::state_path());
    let bytes: u64 = FILES
        .iter()
        .filter_map(|(f, _)| fs::metadata(dir.join(f)).ok())
        .map(|m| m.len())
        .sum();
    bytes / 1_000_000
}

/// The cosine of the angle between `a` and `b`; 0 when either is zero.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
    let n = norm(a) * norm(b);
    if n == 0.0 {
        0.0
    } else {
        dot / n
    }
}
