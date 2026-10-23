mod common;

use common::{person, script, Env};
use lightbulb_connect::daemon::ipc;
use lightbulb_connect::dialog::{self, Queue};
use serde_json::json;
use std::time::Duration;

#[test]
fn ui_claim_returns_its_lease() {
    let env = Env::new();
    env.daemon(false, "", &[]);
    assert_eq!(env.status()["claim_live"], false);
    let r = person(&env, json!({"id": 1, "op": "ui.claim"}));
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["lease_s"], 20);
    assert_eq!(env.status()["claim_live"], true);
}

#[tokio::test]
async fn claim_suppresses_dialog_for_20_seconds() {
    let d = tempfile::tempdir().unwrap();
    // A fake `osascript` that answers at once, noting that it ran.
    let ran = d.path().join("ran");
    let prog = script(
        d.path(),
        &format!("touch {}\necho 'button returned:Allow'", ran.display()),
    );
    let (q, mut rx) = Queue::new(prog);

    let t = 1_000;
    q.claim(t);
    assert!(q.claim_live(t + 19));
    assert!(!q.claim_live(t + 21));

    // A claim made 18 s ago has 2 s left: the dialog waits it out, then shows.
    q.claim(ipc::now() as i64 - 18);
    q.show("1", dialog::args("T", "x", &["Allow"]));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!ran.exists(), "no dialog while claimed");
    let a = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap();
    assert_eq!(a, Some(("1".into(), Some("Allow".into()))));
}

#[test]
fn ui_claim_is_person_only() {
    let env = Env::new();
    // The daemon takes this test process for an agent.
    let me = std::process::id();
    let birth = lightbulb_connect::os::process(me).unwrap().birth_micros;
    env.daemon(
        false,
        "",
        &[("LIGHTBULB_CONNECT_HOOK_CALLER", format!("{me}:{birth}"))],
    );
    let r = env.ask(json!({"id": 1, "op": "ui.claim"}));
    assert_eq!(r["reason"], "not_a_person", "{r}");
    assert_eq!(env.status()["claim_live"], false);
}
