//! The one way the daemon asks the person a question: an `osascript` dialog, one on screen at
//! a time, none while the Lightbulb app holds a `ui.claim` lease.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, Notify};

/// A `ui.claim` lasts this long; the app heartbeats every 7 s.
pub const LEASE_S: i64 = 20;

/// Where answers arrive: `(id, Option<button>)`.
pub type Answers = mpsc::UnboundedReceiver<(String, Option<String>)>;

/// `osascript` arguments for one dialog. The title, text and button names are only ever
/// `argv` items after `--`, never AppleScript source. Callers put `Decline` first: it is the
/// default button, so a stray Enter never allows. Unanswered, it gives up after 600 s.
pub fn args(title: &str, text: &str, buttons: &[&str]) -> Vec<String> {
    let items: Vec<String> = (3..3 + buttons.len())
        .map(|i| format!("item {i} of argv"))
        .collect();
    let show = format!(
        "display dialog (item 2 of argv) with title (item 1 of argv) buttons {{{}}} \
         default button 1 giving up after 600",
        items.join(", ")
    );
    let mut a: Vec<String> = [
        "-e",
        "on run argv",
        "-e",
        &show,
        "-e",
        "end run",
        "--",
        title,
        text,
    ]
    .map(String::from)
    .into();
    a.extend(buttons.iter().map(|b| b.to_string()));
    a
}

/// The button `osascript` printed, or `None` (gave up, or nothing we can read).
fn answer_of(out: &str) -> Option<String> {
    if out.contains("gave up:true") {
        return None;
    }
    let rest = out.split_once("button returned:")?.1;
    let button = rest.split([',', '\n']).next()?.trim();
    (!button.is_empty()).then(|| button.to_string())
}

/// `LIGHTBULB_NETWORK_DIALOG=<button>` (debug builds): every dialog answers that button at
/// once, without a window; an empty value answers `None`.
#[cfg(debug_assertions)]
fn knob() -> Option<Option<String>> {
    std::env::var("LIGHTBULB_NETWORK_DIALOG")
        .ok()
        .map(|b| Some(b).filter(|b| !b.is_empty()))
}

#[cfg(not(debug_assertions))]
fn knob() -> Option<Option<String>> {
    None
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The dialogs waiting and the one on screen. Answers arrive as `(id, Option<button>)`; `None`
/// is "no answer here" (gave up, no GUI, `osascript` failed) and the caller keeps its question
/// open for the CLI. A withdrawn dialog answers nothing.
pub struct Queue(Arc<Inner>);

struct Inner {
    program: PathBuf,
    state: Mutex<State>,
    wake: Notify,
    claim_until: Mutex<i64>,
    answers: mpsc::UnboundedSender<(String, Option<String>)>,
    knob: Option<Option<String>>,
}

#[derive(Default)]
struct State {
    waiting: VecDeque<(String, Vec<String>)>,
    /// The dialog on screen, and what kills it.
    showing: Option<(String, oneshot::Sender<()>)>,
}

impl Queue {
    /// Starts the queue's task; `program` is `osascript` (tests pass a fake).
    pub fn new(program: PathBuf) -> (Queue, Answers) {
        let (answers, rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            program,
            state: Mutex::default(),
            wake: Notify::new(),
            claim_until: Mutex::new(0),
            answers,
            knob: knob(),
        });
        tokio::spawn(run(inner.clone()));
        (Queue(inner), rx)
    }

    /// Queues dialog `id` (`args` from `args`).
    pub fn show(&self, id: &str, args: Vec<String>) {
        if let Some(button) = &self.0.knob {
            let _ = self.0.answers.send((id.into(), button.clone()));
            return;
        }
        self.0
            .state
            .lock()
            .unwrap()
            .waiting
            .push_back((id.into(), args));
        self.0.wake.notify_one();
    }

    /// Takes dialog `id` off the screen or out of the queue; it answers nothing.
    pub fn withdraw(&self, id: &str) {
        let mut s = self.0.state.lock().unwrap();
        s.waiting.retain(|(i, _)| i != id);
        if s.showing.as_ref().is_some_and(|(i, _)| i == id) {
            let (_, kill) = s.showing.take().unwrap();
            let _ = kill.send(());
        }
    }

    /// `ui.claim` at `now`: no dialog opens for `LEASE_S`.
    pub fn claim(&self, now: i64) {
        *self.0.claim_until.lock().unwrap() = now + LEASE_S;
    }

    pub fn claim_live(&self, now: i64) -> bool {
        *self.0.claim_until.lock().unwrap() > now
    }
}

/// One dialog at a time, in order. While a claim is live the head waits, looking again each
/// second, until the claim lapses or it is withdrawn.
async fn run(inner: Arc<Inner>) {
    loop {
        let next = {
            let mut s = inner.state.lock().unwrap();
            if s.waiting.is_empty() || *inner.claim_until.lock().unwrap() > now() {
                Err(s.waiting.is_empty())
            } else {
                let (id, args) = s.waiting.pop_front().unwrap();
                let (kill, killed) = oneshot::channel();
                s.showing = Some((id.clone(), kill));
                Ok((id, args, killed))
            }
        };
        let (id, args, killed) = match next {
            Ok(n) => n,
            Err(true) => {
                inner.wake.notified().await;
                continue;
            }
            Err(false) => {
                let _ = tokio::time::timeout(Duration::from_secs(1), inner.wake.notified()).await;
                continue;
            }
        };
        let child = Command::new(&inner.program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output();
        let answer = tokio::select! {
            out = child => match out {
                Ok(o) if o.status.success() => answer_of(&String::from_utf8_lossy(&o.stdout)),
                Ok(o) => {
                    eprintln!("dialog {id}: osascript {}", o.status);
                    None
                }
                Err(e) => {
                    eprintln!("dialog {id}: no osascript: {}", e.kind());
                    None
                }
            },
            _ = killed => None,
        };
        // Withdrawn (by the kill, or just after it answered): no answer.
        let mine = inner.state.lock().unwrap().showing.take();
        if mine.is_some() {
            let _ = inner.answers.send((id, answer));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::Duration;

    /// A fake `osascript`: notes its dialog's title in `started`, then answers Allow once the
    /// test writes `<title>.answer`.
    fn fake(dir: &Path) -> PathBuf {
        let p = dir.join("osascript");
        let d = dir.display();
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nt=$2\necho \"$t\" >>{d}/started\n\
                 while [ ! -e {d}/$t.answer ]; do sleep 0.02; done\necho 'button returned:Allow, gave up:false'\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn started(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("started")).unwrap_or_default()
    }

    async fn until(what: &str, f: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !f() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out: {what}"));
    }

    fn answer(dir: &Path, title: &str) {
        std::fs::write(dir.join(format!("{title}.answer")), "").unwrap();
    }

    #[test]
    fn dialog_text_is_an_argument() {
        let t = "\" & do shell script \"x\" & \"";
        let a = args("T", t, &["Allow"]);
        let i = a.iter().position(|s| s == "--").unwrap();
        assert!(a[..i].iter().all(|s| !s.contains("do shell")));
        assert!(a[i + 1..].contains(&t.to_string()));
    }

    #[test]
    fn decline_is_the_default_button() {
        let a = args("T", "x", &["Decline", "Allow"]);
        assert!(a.iter().any(|s| s.contains("default button 1")));
        assert!(a.iter().any(|s| s.contains("giving up after 600")));
        assert!(a.ends_with(&["T".into(), "x".into(), "Decline".into(), "Allow".into()]));
    }

    #[tokio::test]
    async fn one_dialog_at_a_time() {
        let d = tempfile::tempdir().unwrap();
        let (q, mut rx) = Queue::new(fake(d.path()));
        q.show("1", args("a", "x", &["Decline", "Allow"]));
        q.show("2", args("b", "x", &["Decline", "Allow"]));
        until("a is shown", || started(d.path()) == "a\n").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(started(d.path()), "a\n", "b waits for a");
        answer(d.path(), "a");
        assert_eq!(rx.recv().await.unwrap(), ("1".into(), Some("Allow".into())));
        until("b is shown", || started(d.path()) == "a\nb\n").await;
        answer(d.path(), "b");
        assert_eq!(rx.recv().await.unwrap(), ("2".into(), Some("Allow".into())));
    }

    #[tokio::test]
    async fn withdraw_dequeues_or_kills() {
        let d = tempfile::tempdir().unwrap();
        let (q, mut rx) = Queue::new(fake(d.path()));
        q.show("1", args("a", "x", &["Allow"]));
        q.show("2", args("b", "x", &["Allow"]));
        q.show("3", args("c", "x", &["Allow"]));
        until("a is shown", || started(d.path()) == "a\n").await;
        q.withdraw("2"); // queued: dequeued
        q.withdraw("1"); // on screen: killed
        until("c is shown", || started(d.path()) == "a\nc\n").await;
        answer(d.path(), "c");
        // Neither withdrawn item answers, and b never showed.
        assert_eq!(rx.recv().await.unwrap(), ("3".into(), Some("Allow".into())));
        assert_eq!(started(d.path()), "a\nc\n");
    }

    #[tokio::test]
    async fn a_failed_dialog_answers_none() {
        let (q, mut rx) = Queue::new("/nonexistent/osascript".into());
        q.show("1", args("a", "x", &["Allow"]));
        assert_eq!(rx.recv().await.unwrap(), ("1".into(), None));
    }

    #[test]
    fn parses_osascript_output() {
        assert_eq!(
            answer_of("button returned:Allow, gave up:false\n"),
            Some("Allow".into())
        );
        assert_eq!(
            answer_of("button returned:Decline\n"),
            Some("Decline".into())
        );
        assert_eq!(answer_of("button returned:, gave up:true\n"), None);
        assert_eq!(answer_of("garbage"), None);
    }
}
