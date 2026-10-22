//! The one way the daemon asks the person a question: an `osascript` dialog, one on screen at
//! a time, none while the Lightbulb app holds a `ui.claim` lease.

use crate::daemon::ipc::now;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, Notify};

/// A `ui.claim` lasts this long; the app heartbeats every 7 s.
pub const LEASE_S: i64 = 20;

/// Where answers arrive: `(id, Option<button>)`.
pub type Answers = mpsc::UnboundedReceiver<(String, Option<String>)>;

/// `osascript` arguments for one dialog. The title, text and button names are only ever
/// `argv` items after `--`, never AppleScript source. Callers put `Decline` first: it is the
/// default button, so a stray Enter never allows. Unanswered, it gives up after 600 s.
/// Callers pass 1-3 fixed button names, `Decline` first. Never `Cancel` (AppleScript treats it
/// as a cancel and `osascript` exits -128), and never a name with a comma (`answer_of` splits
/// on it).
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

/// The dialogs waiting and the one on screen. Answers arrive as `(id, Option<button>)`; `None`
/// is "no answer here" (gave up, no GUI, `osascript` failed) and the caller keeps its question
/// open for the CLI. A withdrawn dialog answers nothing.
pub struct Queue(Arc<Inner>);

struct Inner {
    program: PathBuf,
    state: Mutex<State>,
    wake: Notify,
    claim_until: AtomicI64,
    answers: mpsc::UnboundedSender<(String, Option<String>)>,
    /// `net::Knobs::dialog`: what every dialog answers in place of `osascript`.
    knob: OnceLock<Option<String>>,
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
            claim_until: AtomicI64::new(0),
            answers,
            knob: OnceLock::new(),
        });
        tokio::spawn(run(inner.clone()));
        (Queue(inner), rx)
    }

    /// Debug builds' `LIGHTBULB_NETWORK_DIALOG`: each dialog, in its turn, answers `knob`
    /// without a window.
    pub fn set_knob(&self, knob: Option<Option<String>>) {
        if let Some(k) = knob {
            let _ = self.0.knob.set(k);
        }
    }

    /// Queues dialog `id` (`args` from `args`).
    pub fn show(&self, id: &str, args: Vec<String>) {
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
        self.0.claim_until.store(now + LEASE_S, Relaxed);
    }

    pub fn claim_live(&self, now: i64) -> bool {
        self.0.claim_until.load(Relaxed) > now
    }
}

/// One dialog at a time, in order. While a claim is live the head waits for the claim to
/// lapse (looking again at least each second), or to be withdrawn.
async fn run(inner: Arc<Inner>) {
    loop {
        let left = inner.claim_until.load(Relaxed) - now() as i64;
        let idle = inner.state.lock().unwrap().waiting.is_empty();
        if idle {
            inner.wake.notified().await;
            continue;
        }
        if left > 0 {
            let wait = Duration::from_secs(left.min(1) as u64);
            let _ = tokio::time::timeout(wait, inner.wake.notified()).await;
            continue;
        }
        let (id, args, killed) = {
            let mut s = inner.state.lock().unwrap();
            // Withdrawn since we looked.
            let Some((id, args)) = s.waiting.pop_front() else {
                continue;
            };
            let (kill, killed) = oneshot::channel();
            s.showing = Some((id.clone(), kill));
            (id, args, killed)
        };
        let answer = match inner.knob.get() {
            Some(k) => k.clone(),
            None => {
                let child = Command::new(&inner.program)
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .output();
                tokio::select! {
                    out = child => match out {
                        Ok(o) if o.status.success() => answer_of(&String::from_utf8_lossy(&o.stdout)),
                        other => {
                            let why = other.map(|o| o.status).map_err(|e| e.kind());
                            eprintln!("dialog {id}: no answer: {why:?}");
                            None
                        }
                    },
                    _ = killed => None,
                }
            }
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
