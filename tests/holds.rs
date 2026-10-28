//! Holds: another person's message parked until its person allows it, the answer doors, and
//! expiry. The daemon tests play the teammate (`Peer`); the expiry tests drive `holds` with
//! their own clock.

use lightbulb::daemon::net_ops;
use lightbulb::holds;
use lightbulb::identity::Identity;
use lightbulb::net::frames::Frame;
use lightbulb::roster::{self, Entry, Kind, Roster};
use lightbulb::store::{Session, Store};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

mod common;

use common::{dialog, endpoint, person, seed_team, ulid, until, Env, Peer};

fn envelope(id: &str, rid: &str, from: &str, to: &str, body: &str) -> Frame {
    Frame::Envelope {
        id: id.into(),
        request_id: rid.into(),
        kind: "message".into(),
        from: from.into(),
        to: to.into(),
        reply_to: None,
        thread: None,
        hops: 0,
        body: body.into(),
        files: vec![],
        sent_at: 0,
        meta: Default::default(),
    }
}

impl Peer {
    async fn ack(&mut self, id: &str) -> (String, Option<String>) {
        match self
            .recv_where(|f| matches!(f, Frame::Ack { id: i, .. } if i == id))
            .await
        {
            Frame::Ack { state, reason, .. } => (state, reason),
            _ => unreachable!(),
        }
    }
}

/// A running alone (dialogs answered `button`), its teammate B (another person, not yet
/// allowed) played by the test.
async fn a_and_teammate(button: &str) -> (Env, Env, iroh::Endpoint, Peer) {
    a_and_teammate_with(button, None).await
}

/// `a_and_teammate`, A's daemon also given `pin` (`LIGHTBULB_CONNECT_HOOK_CALLER`).
async fn a_and_teammate_with(
    button: &str,
    pin: Option<(&'static str, String)>,
) -> (Env, Env, iroh::Endpoint, Peer) {
    let (a, b) = (Env::new(), Env::new());
    seed_team(&a, &b);
    let mut env = vec![dialog(button)];
    env.extend(pin);
    a.daemon(false, "", &env);
    let ep = endpoint(&b.identity()).await;
    let mut p = Peer::dial(&ep, &a).await;
    p.hello(vec![]).await;
    until(5, "B online", || a.status()["peers"]["online"] == 1);
    (a, b, ep, p)
}

fn session(e: &Env, conv: &str) -> String {
    format!("{}/codex:{conv}", e.short())
}

/// `e`'s open holds (`holds` is person-only).
fn held(e: &Env) -> Vec<Value> {
    person(e, json!({"id": 1, "op": "holds"}))["holds"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn answer(e: &Env, id: &str, verdict: &str) -> Value {
    person(
        e,
        json!({"id": 1, "op": "hold.answer", "hold": id, "verdict": verdict}),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permission_hold_round_trip() {
    let a = Env::new();
    a.daemon(false, "", &[]);
    let key = register(&a, "claude", "c1", "/tmp/app");
    let mut events = Events::on(&a);

    let r = person(
        &a,
        json!({"id": 1, "op": "hold.file", "kind": "permission", "session": key,
            "prompt": "Run `rm -rf build`?"}),
    );
    assert_eq!(r["ok"], true, "{r}");
    let h = r["hold"].as_str().unwrap().to_string();
    let listed = held(&a);
    assert_eq!(listed[0]["kind"], "permission");
    assert_eq!(listed[0]["preview"], "Run `rm -rf build`?");
    let got = person(&a, json!({"id": 1, "op": "hold.get", "hold": h}));
    let me = lightbulb::os::process(std::process::id()).unwrap();
    assert_eq!(got["target_birth_micros"], me.birth_micros, "{got}");
    assert_eq!(answer(&a, &h, "allow")["ok"], true);
    assert_eq!(
        events.next("hold_closed"),
        json!({"event": "hold_closed", "id": h, "by": "local", "verdict": "allow"})
    );

    // Unanswered for 600 s: expired.
    let (_d, s, ..) = lib();
    let k = "me/claude:c1";
    s.put_session(&row(k)).unwrap();
    let p = holds::file_permission(&s, k, "ok?", 1_000).unwrap();
    assert!(holds::expire(&s, 1_599).unwrap().is_empty());
    assert_eq!(ids(holds::expire(&s, 1_600).unwrap()), [p.id]);
    assert!(holds::open(&s).unwrap().is_empty());
}

/// A store and roster of this computer (`me`) and teammate `aden` (another person), without a
/// daemon.
fn lib() -> (tempfile::TempDir, Store, Roster, Identity, Identity) {
    lib_with(None)
}

/// `lib`, the roster also admitting `extra` (an `admit` body), signed by `me`.
fn lib_with(extra: Option<Value>) -> (tempfile::TempDir, Store, Roster, Identity, Identity) {
    let d = tempfile::tempdir().unwrap();
    let store = Store::open(&d.path().join("state.db")).unwrap();
    let (me, aden) = (common::identity(), common::identity());
    let g = net_ops::genesis(&me, "prasiddha", "mac", "acme", 1);
    let aden_body = json!({"member": aden.key, "name": "aden", "computer": "amac",
        "person_id": aden.key, "kind": "computer", "role": "member"});
    let mut entries = vec![g];
    for body in std::iter::once(aden_body).chain(extra) {
        let parent = entries.last().unwrap().hash();
        entries.push(Entry::signed(&me, vec![parent], Kind::Admit, body, 2));
    }
    (d, store, roster::replay(&entries), me, aden)
}

/// The ids of `hs`.
fn ids(hs: Vec<holds::Hold>) -> Vec<i64> {
    hs.iter().map(|h| h.id).collect()
}

fn row(key: &str) -> Session {
    Session {
        key: key.into(),
        provider: "codex".into(),
        conversation: None,
        pid: Some(1),
        birth_micros: Some(1),
        cwd: "/tmp/app".into(),
        title: None,
        status: "idle".into(),
        seen_at: 0,
        executable: None,
        repo: None,
        branch: None,
        handle: None,
    }
}

/// The subscriber events of `e`'s daemon.
struct Events(BufReader<UnixStream>);

impl Events {
    fn on(e: &Env) -> Events {
        let mut s = UnixStream::connect(e.state.path().join("sock")).unwrap();
        writeln!(s, r#"{{"id":1,"op":"subscribe"}}"#).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        let mut r = BufReader::new(s);
        let mut line = String::new();
        r.read_line(&mut line).unwrap(); // the subscribe answer
        Events(r)
    }

    /// The next `event` named `name`.
    fn next(&mut self, name: &str) -> Value {
        loop {
            let mut line = String::new();
            self.0.read_line(&mut line).unwrap();
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["event"] == name {
                return v;
            }
        }
    }
}

/// Registers `<short>/<provider>:<conv>` on `e` in folder `cwd`, this test process standing in
/// for its agent: its key.
fn register(e: &Env, provider: &str, conv: &str, cwd: &str) -> String {
    let me = lightbulb::os::process(std::process::id()).unwrap();
    let r = e.ask(
        json!({"id": 1, "op": "register", "session": {"provider": provider,
        "conversation": conv, "pid": me.pid, "birth_micros": me.birth_micros, "cwd": cwd}}),
    );
    r["key"].as_str().unwrap().to_string()
}

/// A fake `osascript` in `dir`: each dialog writes its argv to `args.<pid>` and its pid to
/// `pids`, then waits to be killed (withdrawn).
fn fake_osascript(dir: &std::path::Path) -> (&'static str, String) {
    let d = dir.display();
    let p = common::script(
        dir,
        &format!("printf '%s\\n' \"$@\" >{d}/args.$$\necho $$ >>{d}/pids\nexec sleep 600"),
    );
    ("LIGHTBULB_CONNECT_DEBUG_OSASCRIPT", p.display().to_string())
}

/// The fake's one dialog: `(pid, argv after "--")`.
fn the_dialog(dir: &std::path::Path) -> (String, Vec<String>) {
    let mut pid = String::new();
    until(10, "a dialog shows", || {
        pid = std::fs::read_to_string(dir.join("pids")).unwrap_or_default();
        !pid.is_empty() && dir.join(format!("args.{}", pid.trim())).exists()
    });
    let pid = pid.trim().to_string();
    let mut args = vec![];
    until(5, "its argv is written", || {
        let a = std::fs::read_to_string(dir.join(format!("args.{pid}"))).unwrap_or_default();
        args = a.lines().map(String::from).collect();
        args.last()
            .is_some_and(|l| l == "Allow" || l == "Always allow")
    });
    let at = args.iter().position(|a| a == "--").unwrap();
    (pid, args[at + 1..].to_vec())
}

/// Whether process `pid` is gone (or a zombie).
fn gone(pid: &str) -> bool {
    let o = std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&o.stdout);
    stat.trim().is_empty() || stat.trim().starts_with('Z')
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_permission_hold_expires_with_its_session_and_its_dialog_goes() {
    let dir = tempfile::tempdir().unwrap();
    let a = Env::new();
    // Expiry runs every 2 s.
    let fast = ("LIGHTBULB_CONNECT_DEBUG_OUTBOX_S", "2".to_string());
    a.daemon(false, "", &[fake_osascript(dir.path()), fast]);
    let key = register(&a, "claude", "c1", "/tmp/app");
    let mut events = Events::on(&a);
    let r = person(
        &a,
        json!({"id": 1, "op": "hold.file", "kind": "permission", "session": key, "prompt": "ok?"}),
    );
    let h = r["hold"].as_str().unwrap().to_string();
    let (pid, argv) = the_dialog(dir.path());
    assert_eq!(argv[1], "Claude Code asks: \"ok?\"");
    a.ask(json!({"id": 1, "op": "end", "key": key}));
    let closed = events.next("hold_closed");
    assert_eq!(
        closed,
        json!({"event": "hold_closed", "id": h, "by": null, "verdict": "expired"})
    );
    until(10, "the dialog is withdrawn", || gone(&pid));
    assert!(held(&a).is_empty());
    // One for a session with no row is refused outright.
    let r = person(
        &a,
        json!({"id": 1, "op": "hold.file", "kind": "permission",
            "session": format!("{}/claude:none", a.short()), "prompt": "ok?"}),
    );
    assert_eq!(
        (&r["error"], &r["reason"]),
        (&json!("bad_request"), &json!("unknown_session")),
        "{r}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hold_answer_on_an_instance_needs_decided_by() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, token, sock) = common::member(dir.path(), json!([]));
    for decided_by in [Value::Null, json!(""), json!(3)] {
        let body = json!({"hold": "H1", "verdict": "allow", "decided_by": decided_by});
        let r = common::line(&sock, &token, "hold.answer", body);
        assert_eq!(r["error"], "bad_request", "{decided_by}: {r}");
    }
    common::ok(&sock, &token, "stop", json!({}));
    assert!(child.wait().unwrap().success());
}

/// Another person's message is stored at once: membership is the only gate, and nothing is
/// held for an Allow.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_teammates_message_is_stored_not_held() {
    let (a, b, _ep, mut p) = a_and_teammate("").await;
    let id = ulid();
    p.send(&envelope(
        &id,
        "r1",
        &session(&b, "b1"),
        &session(&a, "a1"),
        "hi",
    ))
    .await;
    assert_eq!(p.ack(&id).await, ("stored".into(), None));
    assert!(held(&a).is_empty());
}
