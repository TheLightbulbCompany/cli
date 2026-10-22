mod common;

use common::Env;
use lightbulb_network::dialog::{self, Queue};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[test]
fn ui_claim_returns_its_lease() {
    let env = Env::new();
    env.daemon(false, "", &[]);
    assert_eq!(env.status()["claim_live"], false);
    let r = env.ask(json!({"id": 1, "op": "ui.claim"}));
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["lease_s"], 20);
    assert_eq!(env.status()["claim_live"], true);
}

#[tokio::test]
async fn claim_suppresses_dialog_for_20_seconds() {
    let d = tempfile::tempdir().unwrap();
    // A fake `osascript` that answers at once, noting that it ran.
    let prog = d.path().join("osascript");
    std::fs::write(
        &prog,
        format!(
            "#!/bin/sh\ntouch {}/ran\necho 'button returned:Allow'\n",
            d.path().display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (q, mut rx) = Queue::new(prog);

    let t = 1_000;
    q.claim(t);
    assert!(q.claim_live(t + 19));
    assert!(!q.claim_live(t + 21));

    // A claim made 18 s ago has 2 s left: the dialog waits it out, then shows.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    q.claim(now - 18);
    q.show("1", dialog::args("T", "x", &["Allow"]));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!d.path().join("ran").exists(), "no dialog while claimed");
    let a = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap();
    assert_eq!(a, Some(("1".into(), Some("Allow".into()))));
}
