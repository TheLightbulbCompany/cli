//! The uploader in the daemon: closed sessions go to Lightbulb only when the person chose
//! `metadata` or `full`, signed with this computer's identity, and the watermark moves only
//! after a 2xx.

mod common;

use common::{api_stub, Env, Req, Seen};
use lightbulb::capture::{self, SessionRow};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const EVERY_MS: &str = "150";

fn closed(id: &str, started: i64, ended: i64) -> SessionRow {
    SessionRow {
        id: id.into(),
        harness: "claude".into(),
        computer: "mac".into(),
        repo: Some("acme/app".into()),
        branch: Some("main".into()),
        started,
        ended: Some(ended),
        prompt_tokens: Some(10),
        completion_tokens: Some(4),
    }
}

/// Signed in, two closed sessions (one with a tool call) and an open one stored, the watermark
/// at 0, a daemon on `mode` talking to `answer`'s stub.
fn run(mode: &str, answer: impl Fn(&Req, usize) -> (u16, Value) + Send + 'static) -> (Env, Seen) {
    let e = Env::new();
    e.sign_in("user_1", None);
    let store = e.store();
    for s in [closed("claude:a", 100, 200), closed("claude:b", 150, 300)] {
        capture::start_session(
            &store,
            &SessionRow {
                ended: None,
                ..s.clone()
            },
            "/work",
        )
        .unwrap();
        capture::end_session(&store, &s.id, s.ended.unwrap()).unwrap();
        capture::set_tokens(&store, &s.id, Some(10), Some(4)).unwrap();
    }
    let open = SessionRow {
        ended: None,
        ..closed("claude:open", 400, 0)
    };
    capture::start_session(&store, &open, "/work").unwrap();
    capture::record_tool_call(
        &store,
        "claude:a",
        "Bash",
        r#"{"command":"cargo test"}"#,
        Some(120),
        true,
    )
    .unwrap();
    drop(store);
    std::fs::write(e.state.path().join("upload.mark"), "0").unwrap();
    let (base, seen) = api_stub(answer);
    e.daemon(
        false,
        &format!("capture_upload = \"{mode}\"\n"),
        &[
            ("LIGHTBULB_API", base),
            ("LIGHTBULB_CONNECT_DEBUG_UPLOAD_MS", EVERY_MS.into()),
        ],
    );
    (e, seen)
}

fn uploads(seen: &Seen) -> Vec<Req> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|r| r.path == "/sessions")
        .cloned()
        .collect()
}

fn wait_for(seen: &Seen, n: usize) {
    let end = Instant::now() + Duration::from_secs(10);
    while uploads(seen).len() < n {
        assert!(
            Instant::now() < end,
            "expected {n} uploads, saw {:?}",
            seen.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn stored(_: &Req, _: usize) -> (u16, Value) {
    (200, json!({"stored": 2}))
}

#[test]
fn test_upload_mode_off_stops_uploads() {
    let (_e, seen) = run("off", stored);
    std::thread::sleep(Duration::from_millis(1200)); // many ticks
    assert!(
        seen.lock().unwrap().is_empty(),
        "off sends no request at all: {:?}",
        seen.lock().unwrap()
    );
}

#[test]
fn upload_metadata_omits_summaries() {
    let (_e, seen) = run("metadata", stored);
    wait_for(&seen, 1);
    let body = uploads(&seen)[0].body.clone();
    assert_eq!(body["mode"], "metadata");
    let ids: Vec<&str> = body["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["claude_a", "claude_b"], "closed sessions only");
    assert_eq!(body["sessions"][0]["prompt_tokens"], 10);
    let calls = body["tool_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["tool"], "Bash");
    assert!(calls[0]["summary"].is_null(), "{calls:?}");
}

#[test]
fn upload_full_sends_summaries_and_only_once() {
    let (_e, seen) = run("full", stored);
    wait_for(&seen, 1);
    let body = uploads(&seen)[0].body.clone();
    assert_eq!(body["mode"], "full");
    assert!(body["tool_calls"][0]["summary"]
        .as_str()
        .unwrap()
        .contains("cargo test"));
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(uploads(&seen).len(), 1, "the watermark moved after the 2xx");
}

#[test]
fn uploader_signs_batches() {
    // The first answer is a failure: the same batch comes again.
    let (e, seen) = run("metadata", |_, n| {
        if n == 0 {
            (500, json!({}))
        } else {
            (200, json!({"stored": 2}))
        }
    });
    wait_for(&seen, 2);
    let sent = uploads(&seen);
    assert_eq!(sent[0].body, sent[1].body, "no watermark after a 500");
    let req = &sent[1];
    assert_eq!(req.method, "POST");
    let proof = std::fs::read(e.state.path().join("login.json")).unwrap();
    let proof: Value = serde_json::from_slice(&proof).unwrap();
    assert_eq!(req.header("x-lightbulb-proof"), proof["proof"].as_str());
    // The challenge is "<ts> POST <path> <sha256(body)> <label>" and the signature is this
    // computer's, over the same message the MCP server's calls sign.
    let challenge = String::from_utf8(
        data_encoding::BASE64URL_NOPAD
            .decode(req.header("x-lightbulb-challenge").unwrap().as_bytes())
            .unwrap(),
    )
    .unwrap();
    let parts: Vec<&str> = challenge.split(' ').collect();
    assert_eq!((parts[1], parts[2]), ("POST", "/sessions"), "{challenge}");
    let id = e.identity();
    let signed = lightbulb::login::proof(&id, "tools", &challenge, None);
    assert!(lightbulb::identity::verify(
        &id.key,
        &signed,
        req.header("x-lightbulb-sig").unwrap()
    ));
    // Then a later tick finds nothing new.
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(uploads(&seen).len(), 2);
}

#[test]
fn nothing_uploads_when_signed_out() {
    let e = Env::new();
    let (base, seen) = api_stub(stored);
    e.daemon(
        false,
        "capture_upload = \"full\"\n",
        &[
            ("LIGHTBULB_API", base),
            ("LIGHTBULB_CONNECT_DEBUG_UPLOAD_MS", EVERY_MS.into()),
        ],
    );
    std::thread::sleep(Duration::from_millis(800));
    assert!(seen.lock().unwrap().is_empty());
}
