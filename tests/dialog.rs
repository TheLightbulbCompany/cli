//! The dialog queue against a fake `osascript`. Multi-threaded: `until` blocks its thread.
mod common;

use common::{script, until};
use lightbulb_connect::dialog::{args, Queue};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Notes its dialog's title in `started`, then answers Allow once `<title>.answer` exists.
fn fake(dir: &Path) -> PathBuf {
    let d = dir.display();
    script(
        dir,
        &format!(
            "while [ \"$1\" != -- ]; do shift; done\nt=$2\necho \"$t\" >>{d}/started\n\
             while [ ! -e {d}/$t.answer ]; do sleep 0.02; done\n\
             echo 'button returned:Allow, gave up:false'"
        ),
    )
}

fn started(d: &Path) -> String {
    std::fs::read_to_string(d.join("started")).unwrap_or_default()
}

fn answer(d: &Path, title: &str) {
    std::fs::write(d.join(format!("{title}.answer")), "").unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn one_dialog_at_a_time() {
    let d = tempfile::tempdir().unwrap();
    let (q, mut rx) = Queue::new(fake(d.path()));
    q.show("1", args("a", "x", &["Decline", "Allow"]));
    q.show("2", args("b", "x", &["Decline", "Allow"]));
    until(10, "a is shown", || started(d.path()) == "a\n");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(started(d.path()), "a\n", "b waits for a");
    answer(d.path(), "a");
    assert_eq!(rx.recv().await.unwrap(), ("1".into(), Some("Allow".into())));
    until(10, "b is shown", || started(d.path()) == "a\nb\n");
    answer(d.path(), "b");
    assert_eq!(rx.recv().await.unwrap(), ("2".into(), Some("Allow".into())));
}

#[tokio::test(flavor = "multi_thread")]
async fn withdraw_dequeues_or_kills() {
    let d = tempfile::tempdir().unwrap();
    let (q, mut rx) = Queue::new(fake(d.path()));
    q.show("1", args("a", "x", &["Allow"]));
    q.show("2", args("b", "x", &["Allow"]));
    q.show("3", args("c", "x", &["Allow"]));
    until(10, "a is shown", || started(d.path()) == "a\n");
    q.withdraw("2"); // queued: dequeued
    q.withdraw("1"); // on screen: killed
    until(10, "c is shown", || started(d.path()) == "a\nc\n");
    answer(d.path(), "c");
    // Neither withdrawn item answers, and b never showed.
    assert_eq!(rx.recv().await.unwrap(), ("3".into(), Some("Allow".into())));
    assert_eq!(started(d.path()), "a\nc\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_dialog_answers_none() {
    let (q, mut rx) = Queue::new("/nonexistent/osascript".into());
    q.show("1", args("a", "x", &["Allow"]));
    assert_eq!(rx.recv().await.unwrap(), ("1".into(), None));
}
