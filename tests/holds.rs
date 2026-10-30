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
        reason: None,
        ended_at: None,
        waiting_for: None,
        headless: false,
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

/// A dialog that gave up unanswered comes back when the person looks (`lightbulb held`), and
/// one already on screen is not shown twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dialog_that_gave_up_is_shown_again_by_held() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display();
    // The first dialog gives up at once; any later one stays up.
    let fake = common::script(
        dir.path(),
        &format!(
            "printf '%s\\n' \"$@\" >{d}/args.$$\necho $$ >>{d}/pids\n\
             if [ \"$(wc -l <{d}/pids)\" -eq 1 ]; then echo 'button returned:, gave up:true'; exit 0; fi\n\
             exec sleep 600"
        ),
    );
    let a = Env::new();
    a.daemon(
        false,
        "",
        &[(
            "LIGHTBULB_CONNECT_DEBUG_OSASCRIPT",
            fake.display().to_string(),
        )],
    );
    let key = register(&a, "claude", "c1", "/tmp/app");
    let r = person(
        &a,
        json!({"id": 1, "op": "hold.file", "kind": "permission", "session": key, "prompt": "ok?"}),
    );
    assert_eq!(r["ok"], true, "{r}");
    let pids = || std::fs::read_to_string(dir.path().join("pids")).unwrap_or_default();
    until(10, "the first dialog gives up", || {
        pids().lines().next().is_some_and(|p| gone(p.trim()))
    });
    std::thread::sleep(Duration::from_millis(500));
    let o = a.detached(&[common::EXE, "held"], "");
    assert!(o.status.success(), "{}", common::err(&o));
    until(10, "the dialog is shown again", || {
        pids().lines().count() == 2
    });
    let second = pids().lines().nth(1).unwrap().to_string();
    let args = std::fs::read_to_string(dir.path().join(format!("args.{second}"))).unwrap();
    assert!(args.contains("Claude Code asks: \"ok?\""), "{args}");
    // On screen now: a second look shows nothing new.
    let o = a.detached(&[common::EXE, "held"], "");
    assert!(o.status.success(), "{}", common::err(&o));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(pids().lines().count(), 2);
    assert_eq!(held(&a).len(), 1);
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

// ---- worker holds (network workers, plan 2, task 2) ----

use lightbulb::mail::Meta;
use lightbulb::store::{Message, Worker};
use lightbulb::workers::{self, Policy};
use std::path::Path;

/// A worker-start hold from `from` (a session key) to `<me>/claude:new`, brief `body`.
fn assign_hold(
    s: &Store,
    from: &str,
    member: &str,
    to: &str,
    deadline: Option<i64>,
) -> holds::Hold {
    let id = ulid();
    let m = Message {
        request_id: format!("r-{id}"),
        id,
        from_key: from.into(),
        to_key: to.into(),
        kind: "assign".into(),
        thread: None,
        hops: 0,
        origin: None,
        body: "fix the build".into(),
        sent_at: 1,
        reply_to: None,
        meta: Meta {
            deadline_at: deadline,
            ..Default::default()
        },
    };
    s.put_message(&m).unwrap();
    holds::insert(s, m, member, 1_000).unwrap()
}

/// A running worker `session` (with a registered row) for the assignment of `hold`.
fn running_worker(s: &Store, session: &str, hold: &holds::Hold) {
    let mut r = row(session);
    r.provider = "claude".into();
    s.put_session(&r).unwrap();
    s.insert_worker(&Worker {
        id: 0,
        assignment: hold.message.id.clone(),
        session: session.into(),
        provider: "claude".into(),
        folder: "/w/r1".into(),
        pid: Some(1),
        started_at: 1,
        ended_at: None,
        state: "running".into(),
        by: None,
    })
    .unwrap();
}

const FOLDER: &str = "/w";

fn dlg(s: &Store, r: &Roster, me: &Identity, h: &holds::Hold) -> (String, Vec<&'static str>) {
    let (t, b, _) = holds::dialog(s, r, &me.key, h, Path::new(FOLDER));
    (t, b)
}

#[allow(clippy::too_many_arguments)]
fn ans(
    s: &Store,
    r: &Roster,
    me: &Identity,
    h: &holds::Hold,
    scope: &str,
    by: &str,
    by_agent: bool,
    now: i64,
) -> Result<(), &'static str> {
    holds::answer(
        s,
        r,
        me,
        Path::new(FOLDER),
        h.id,
        true,
        scope,
        by,
        by_agent,
        now,
    )
    .unwrap()
    .map(|_| ())
}

#[test]
fn a_hold_on_an_assign_is_a_worker_hold() {
    let (_d, s, r, me, aden) = lib();
    let from = format!("{}/codex:a1", aden.short);
    let h = assign_hold(
        &s,
        &from,
        &aden.key,
        &format!("{}/claude:new", me.short),
        None,
    );
    assert_eq!(h.kind(), holds::Kind::Worker);
    let (text, buttons) = dlg(&s, &r, &me, &h);
    assert!(text.starts_with("aden"), "{text}");
    let dir = format!(
        "wants to start a Claude Code worker in /w/{}:",
        h.message.id
    );
    assert!(text.contains(&dir), "{text}");
    assert!(
        text.ends_with(
            ": \"fix the build\". Always allow lets aden's computer start workers here \
             without asking."
        ),
        "{text}"
    );
    assert_eq!(buttons, ["Decline", "Allow", "Always allow"]);
}

#[test]
fn a_worker_prompt_hold_shows_the_worker_and_three_buttons() {
    let (_d, s, r, me, aden) = lib();
    let from = format!("{}/codex:a1", aden.short);
    let w = format!("{}/claude:w1", me.short);
    let a = assign_hold(
        &s,
        &from,
        &aden.key,
        &format!("{}/claude:new", me.short),
        None,
    );
    running_worker(&s, &w, &a);
    let p = holds::file_permission(&s, &w, "Bash: rm -rf build", 1_000).unwrap();
    assert!(holds::is_workers(&s, &w));
    let (text, buttons) = dlg(&s, &r, &me, &p);
    assert!(
        text.starts_with("worker claude-1 (claude) for aden"),
        "{text}"
    );
    assert!(
        text.ends_with(" in /w/r1 wants to: Bash: rm -rf build"),
        "{text}"
    );
    assert_eq!(buttons, ["Decline", "Allow", "Stop worker"]);
}

#[test]
fn an_allow_after_the_deadline_is_not_open() {
    let (_d, s, r, me, aden) = lib();
    let from = format!("{}/codex:a1", aden.short);
    let h = assign_hold(
        &s,
        &from,
        &aden.key,
        &format!("{}/claude:new", me.short),
        Some(2_000),
    );
    // Not due yet: still open. At the deadline an Allow is refused even before the timer ran.
    assert!(holds::expire(&s, 1_999).unwrap().is_empty());
    assert_eq!(
        ans(&s, &r, &me, &h, "once", "local", false, 2_000),
        Err("not_open")
    );
    assert_eq!(ids(holds::expire(&s, 2_000).unwrap()), [h.id]);
    assert_eq!(holds::get(&s, h.id).unwrap().unwrap().state, "expired");
    let (state, reason): (String, Option<String>) =
        rusqlite::Connection::open(_d.path().join("state.db"))
            .unwrap()
            .query_row(
                "SELECT state, reason FROM receipts WHERE from_key = ?1 AND request_id = ?2",
                [&h.from_session, &h.message.request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
    assert_eq!(
        (state.as_str(), reason.as_deref()),
        ("expired", Some("timed_out"))
    );
    assert_eq!(
        ans(&s, &r, &me, &h, "once", "local", false, 2_001),
        Err("not_open")
    );
}

#[test]
fn a_workers_prompt_hold_outlives_permission_s() {
    let (_d, s, _r, me, aden) = lib();
    let from = format!("{}/codex:a1", aden.short);
    let w = format!("{}/claude:w1", me.short);
    let a = assign_hold(
        &s,
        &from,
        &aden.key,
        &format!("{}/claude:new", me.short),
        None,
    );
    running_worker(&s, &w, &a);
    let p = holds::file_permission(&s, &w, "ok?", 1_000).unwrap();
    assert!(holds::expire(&s, 1_000 + holds::PERMISSION_S + 1)
        .unwrap()
        .is_empty());
    // Gone with its session.
    s.end_session(&w, 5_000).unwrap();
    assert_eq!(ids(holds::expire(&s, 5_001).unwrap()), [p.id]);
}

#[test]
fn an_agent_cannot_answer_a_workers_holds() {
    let (_d, s, r, me, aden) = lib();
    let from = format!("{}/codex:a1", aden.short);
    let w = format!("{}/claude:w1", me.short);
    let agent = format!("{}/claude-3", me.short);
    let a = assign_hold(
        &s,
        &from,
        &aden.key,
        &format!("{}/claude:new", me.short),
        None,
    );
    running_worker(&s, &w, &a);
    let p = holds::file_permission(&s, &w, "ok?", 1_000).unwrap();
    for h in [&a, &p] {
        assert_eq!(
            ans(&s, &r, &me, h, "once", &agent, true, 1_001),
            Err("not_a_person")
        );
        assert_eq!(
            ans(&s, &r, &me, h, "once", "agent", true, 1_001),
            Err("not_a_person")
        );
        assert!(holds::get(&s, h.id).unwrap().unwrap().is_open());
    }
    // A plain app-filed prompt still takes an agent, and a person takes a worker's.
    let k = format!("{}/codex:c1", me.short);
    s.put_session(&row(&k)).unwrap();
    let plain = holds::file_permission(&s, &k, "ok?", 1_000).unwrap();
    assert!(ans(&s, &r, &me, &plain, "once", &agent, true, 1_001).is_ok());
    // The flag decides, not the shape of `by`: a person named `acme/jo` is accepted.
    assert!(ans(&s, &r, &me, &p, "once", "acme/jo", false, 1_001).is_ok());
    assert!(ans(&s, &r, &me, &a, "once", "local", false, 1_001).is_ok());
}

#[test]
fn always_allow_writes_the_policy() {
    let (_d, s, r, me, aden) = lib();
    let from = format!("{}/codex:a1", aden.short);
    let to = format!("{}/claude:new", me.short);
    assert_eq!(
        workers::policy(&s, &me.key, &aden.key).unwrap(),
        Policy::Ask
    );
    let once = assign_hold(&s, &from, &aden.key, &to, None);
    assert!(ans(&s, &r, &me, &once, "once", "local", false, 1_001).is_ok());
    assert_eq!(
        workers::policy(&s, &me.key, &aden.key).unwrap(),
        Policy::Ask
    );
    let h = assign_hold(&s, &from, &aden.key, &to, None);
    assert!(ans(&s, &r, &me, &h, "always", "local", false, 1_002).is_ok());
    assert_eq!(
        workers::policy(&s, &me.key, &aden.key).unwrap(),
        Policy::Allow
    );
    let scope: String = rusqlite::Connection::open(_d.path().join("state.db"))
        .unwrap()
        .query_row(
            "SELECT scope FROM approvals WHERE hold_id = ?1",
            [h.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(scope, "always");
    // "always" means nothing on a prompt or a decline.
    let k = format!("{}/codex:c1", me.short);
    s.put_session(&row(&k)).unwrap();
    let plain = holds::file_permission(&s, &k, "ok?", 1_000).unwrap();
    assert_eq!(
        ans(&s, &r, &me, &plain, "always", "local", false, 1_003),
        Err("bad_request")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lightbulb_held_prints_the_dialog_text_and_the_kind() {
    let a = Env::new();
    a.daemon(false, "", &[]);
    let key = register(&a, "claude", "c1", "/tmp/app");
    let r = person(
        &a,
        json!({"id": 1, "op": "hold.file", "kind": "permission", "session": key, "prompt": "ok?"}),
    );
    assert_eq!(r["ok"], true, "{r}");
    let o = a.detached(&[common::EXE, "held"], "");
    assert_eq!(
        common::out(&o),
        "H1  permission  Claude Code asks: \"ok?\"  — lightbulb approve H1\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approve_always_is_an_allow_with_scope_always() {
    let a = Env::new();
    // A worker-start hold seeded before the daemon starts (nothing creates one yet).
    let db = a.state.path().join("state.db");
    {
        let s = Store::open(&db).unwrap();
        assign_hold(&s, "bob/codex:b1", "bobkey", "me/claude:new", None);
    }
    // Offered (a start hold for a harness not offered is declined at start); never the real
    // Claude Code.
    let config = "network_workers = true\nworkers_offer = \"claude\"\n";
    let never = (
        "LIGHTBULB_CONNECT_DEBUG_CLAUDE",
        "/nonexistent/claude".to_string(),
    );
    a.daemon(false, config, &[never]);
    let o = a.detached(&[common::EXE, "approve", "H1", "--always"], "");
    assert_eq!(
        common::out(&o),
        "Allowed H1 (always).\n",
        "{}",
        common::err(&o)
    );
    let scope: String = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("SELECT scope FROM approvals WHERE hold_id = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let s = Store::open(&db).unwrap();
    assert_eq!(scope, "always");
    assert_eq!(workers::policy(&s, "x", "bobkey").unwrap(), Policy::Allow);
}
