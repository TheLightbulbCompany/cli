//! `lightbulb member`: the engine as a cloud workspace's sidecar, one more computer on its
//! team's network, driven by the Lightbulb bridge over a token-gated Unix socket.

mod common;

use common::{identity, line, member, until, Peer};
use iroh::endpoint::VarInt;
use lightbulb::identity::Identity;
use lightbulb::net::frames::Frame;
use lightbulb::net::SUPERSEDED;
use lightbulb::roster::{self, Entry, Kind};
use lightbulb::store::Store;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Child;
use std::time::Duration;

const NAME: &str = "The Lightbulb Company";

fn friday() -> Value {
    json!([{"name": "friday", "agent": "hermes", "title": "Friday", "agentId": "a-1"}])
}

/// A team stored in `dir/home` before the member starts: A ("prasiddha" on "mac", founder,
/// workspace W1) and the member itself, W1's cloud member. Returns A.
fn seed(dir: &Path) -> Identity {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let w = Identity::load_or_create(&home).unwrap();
    let a = identity();
    let person = format!("p-{}", a.short);
    let g = Entry::signed(
        &a,
        vec![],
        Kind::Admit,
        json!({"member": a.key, "name": "prasiddha", "computer": "mac", "kind": "computer",
            "role": "founder", "ticket_id": "", "team_name": "acme", "person_id": person,
            "proof": common::person_proof(&a, &person, "prasiddha", Some("W1"))}),
        1,
    );
    let aw = common::admit(
        &a,
        &g,
        &w,
        NAME,
        json!({"kind": "cloud", "person_id": "ws:W1", "proof": common::ws_proof(&w, "W1", NAME)}),
    );
    let store = Store::open(&home.join("state.db")).unwrap();
    for e in [&g, &aw] {
        roster::accept(&store, e, 10).unwrap();
    }
    a
}

/// The answer's `result` (the op must succeed).
fn ok(sock: &Path, token: &str, op: &str, body: Value) -> Value {
    let r = line(sock, token, op, body);
    assert_eq!(
        (r["ok"].clone(), r["id"].clone()),
        (json!(true), json!(7)),
        "{op}: {r}"
    );
    r["result"].clone()
}

/// `who` dialing the member behind `sock`: the connection, `hello` sent.
async fn dial(sock: &Path, token: &str, who: &Identity) -> (Peer, iroh::Endpoint) {
    let key = ok(sock, token, "identity.get", json!({}))["member_key"].clone();
    let port = ok(sock, token, "status", json!({}))["port"]
        .as_u64()
        .unwrap();
    common::dial_at(key.as_str().unwrap(), port, who).await
}

/// `p` (member `who`) sends `text` to `to`: its envelope id and the member's ack state.
async fn send(p: &mut Peer, who: &Identity, to: &str, text: &str) -> (String, String) {
    let id = common::ulid();
    p.send(&Frame::Envelope {
        id: id.clone(),
        request_id: common::ulid(),
        kind: "message".into(),
        from: format!("{}/codex:c1", who.short),
        to: to.into(),
        reply_to: None,
        thread: None,
        hops: 0,
        body: text.into(),
        files: vec![],
        sent_at: 0,
    })
    .await;
    match p
        .recv_where(|f| matches!(f, Frame::Ack { id: i, .. } if *i == id))
        .await
    {
        Frame::Ack { state, .. } => (id, state),
        _ => unreachable!(),
    }
}

/// The receipt state `p` hears next for envelope `id`.
async fn receipt(p: &mut Peer, id: &str) -> String {
    match p
        .recv_where(|f| matches!(f, Frame::Receipt { id: i, .. } if i == id))
        .await
    {
        Frame::Receipt { state, .. } => state,
        _ => unreachable!(),
    }
}

fn exits_zero(mut child: Child) {
    let mut code = None;
    until(10, "the member exits", || {
        code = child.try_wait().unwrap();
        code.is_some()
    });
    assert!(code.unwrap().success(), "{code:?}");
}

fn short(sock: &Path, token: &str) -> String {
    ok(sock, token, "identity.get", json!({}))["member_key_short"]
        .as_str()
        .unwrap()
        .into()
}

#[test]
fn a_line_without_the_token_is_refused_and_closed() {
    let dir = tempfile::tempdir().unwrap();
    let (child, token, sock) = member(dir.path(), json!([]));
    for req in [
        json!({"id": 3, "op": "status"}),
        json!({"id": 3, "op": "status", "token": "wrong"}),
        json!({"id": 3, "op": "status", "token": format!("{token}x")}),
    ] {
        let mut s = UnixStream::connect(&sock).unwrap();
        writeln!(s, "{req}").unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut answer = String::new();
        r.read_line(&mut answer).unwrap();
        let answer: Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(
            answer,
            json!({"id": 3, "ok": false, "error": "unauthorized"})
        );
        // Closed: the next line gets nothing back.
        let _ = writeln!(s, "{}", json!({"id": 4, "op": "status", "token": token}));
        let mut more = String::new();
        assert_eq!(r.read_line(&mut more).unwrap_or(0), 0, "{more}");
    }
    // A line longer than 1 MiB is not read to its end: refused and closed.
    let mut s = UnixStream::connect(&sock).unwrap();
    let long = format!(
        "{{\"token\": \"{token}\", \"pad\": \"{}\"}}\n",
        "x".repeat(2 << 20)
    );
    let _ = s.write_all(long.as_bytes()); // the member may close before it is all written
    let mut answer = String::new();
    BufReader::new(s).read_line(&mut answer).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&answer).unwrap()["error"],
        "unauthorized"
    );
    assert_eq!(ok(&sock, &token, "ping", json!({})), json!({}));
    // SAFETY: kill takes a pid and a signal; the pid is our own live child's.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    exits_zero(child);
}

#[test]
fn status_answers_as_an_instance_and_shutdown_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (child, token, sock) = member(dir.path(), json!([]));
    let s = ok(&sock, &token, "status", json!({}));
    assert_eq!(s["version"], env!("CARGO_PKG_VERSION"), "{s}");
    assert_eq!(s["team"]["name"], NAME, "{s}");
    let r = line(&sock, &token, "shutdown", json!({}));
    assert_eq!(
        (r["ok"].clone(), r["error"].clone(), r["id"].clone()),
        (json!(false), json!("unsupported"), json!(7)),
        "{r}"
    );
    // A person-only op answers as for an instance's caller.
    let r = line(&sock, &token, "team.invite", json!({}));
    assert_eq!(r["error"], "unsupported", "{r}");
    // Still running: SIGTERM stops it, exit 0, with its snapshot.
    assert_eq!(ok(&sock, &token, "ping", json!({})), json!({}));
    // SAFETY: kill takes a pid and a signal; the pid is our own live child's.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    exits_zero(child);
    assert!(dir.path().join("last.db").is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_early_message_to_a_declared_session_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let a = seed(dir.path());
    // A bad name and a second friday are skipped: the member starts with friday declared.
    let mut sessions = friday();
    let list = sessions.as_array_mut().unwrap();
    list.push(json!({"name": "Not a name!", "agent": "hermes"}));
    list.push(json!({"name": "friday", "agent": "other"}));
    let (child, token, sock) = member(dir.path(), sessions);
    // No `cloud.sessions.set`: the sessions file declared friday before the member started.
    let (mut pa, _ep) = dial(&sock, &token, &a).await;
    let to = format!("{}/friday", short(&sock, &token));
    assert_eq!(send(&mut pa, &a, &to, "early").await.1, "stored");
    let ghost = format!("{}/ghost", short(&sock, &token));
    assert_eq!(send(&mut pa, &a, &ghost, "nobody").await.1, "refused");
    let peek = ok(&sock, &token, "inbox.peek", json!({}));
    assert_eq!(peek["sessions"][0]["session"], "friday", "{peek}");
    // Friday's agent was removed: its mail stays with the engine, off the peek.
    ok(&sock, &token, "cloud.sessions.set", json!({"sessions": []}));
    let peek = ok(&sock, &token, "inbox.peek", json!({}));
    assert_eq!(peek["sessions"], json!([]), "{peek}");
    ok(&sock, &token, "stop", json!({}));
    exits_zero(child);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peek_lists_without_taking_and_ack_moves_the_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let a = seed(dir.path());
    let (child, token, sock) = member(dir.path(), friday());
    let (mut pa, _ep) = dial(&sock, &token, &a).await;
    let to = format!("{}/friday", short(&sock, &token));
    let (id, state) = send(&mut pa, &a, &to, "hello friday").await;
    assert_eq!(state, "stored");

    let peek = ok(&sock, &token, "inbox.peek", json!({}));
    assert_eq!(
        peek,
        ok(&sock, &token, "inbox.peek", json!({})),
        "peek takes nothing"
    );
    let groups = peek["sessions"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "{peek}");
    assert_eq!(groups[0]["session"], "friday");
    let records = groups[0]["records"].as_array().unwrap();
    assert_eq!(records.len(), 1, "{peek}");
    let rec = &records[0];
    assert_eq!(
        (rec["id"].as_str(), rec["text"].as_str()),
        (Some(id.as_str()), Some("hello friday"))
    );
    assert!(
        rec["framed"].as_str().unwrap().contains("hello friday"),
        "{rec}"
    );
    let mut keys: Vec<&String> = rec.as_object().unwrap().keys().collect();
    keys.sort();
    let mut want = lightbulb::daemon::ipc::INBOX_RECORD.to_vec();
    want.push("stage");
    want.sort();
    assert_eq!(keys, want);
    assert_eq!(rec["stage"], "waiting");

    let ack = |record: &str, stage: &str| {
        line(
            &sock,
            &token,
            "inbox.ack",
            json!({"session": "friday", "record": record, "stage": stage}),
        )
    };
    assert_eq!(ack(&id, "seen")["error"], "bad_request");
    assert_eq!(ack("nope", "delivered")["error"], "not_found");
    assert_eq!(ack("nope", "read")["error"], "not_found");

    assert_eq!(
        ack(&id, "delivered"),
        json!({"id": 7, "ok": true, "result": {}})
    );
    assert_eq!(receipt(&mut pa, &id).await, "delivered");
    // Delivered, not read (a turn that died): still listed, and a second ack is the same.
    let after = ok(&sock, &token, "inbox.peek", json!({}));
    let listed = &after["sessions"][0]["records"];
    assert_eq!(listed.as_array().map(Vec::len), Some(1), "{after}");
    assert_eq!(
        (listed[0]["id"].as_str(), listed[0]["stage"].as_str()),
        (Some(id.as_str()), Some("delivered"))
    );
    assert_eq!(ack(&id, "delivered")["result"], json!({}));

    assert_eq!(ack(&id, "read")["ok"], true);
    assert_eq!(receipt(&mut pa, &id).await, "read");
    let after = ok(&sock, &token, "inbox.peek", json!({}));
    assert_eq!(after["sessions"], json!([]), "{after}");
    // `read` on a record still waiting takes it too.
    let (id2, _) = send(&mut pa, &a, &to, "second").await;
    assert_eq!(ack(&id2, "read")["ok"], true);
    // Its receipt reaches `read` (a `delivered` may come first or be folded into it).
    pa.recv_where(
        |f| matches!(f, Frame::Receipt { id, state, .. } if *id == id2 && state == "read"),
    )
    .await;
    let after = ok(&sock, &token, "inbox.peek", json!({}));
    assert_eq!(after["sessions"], json!([]), "{after}");
    let db = dir.path().join("home/state.db");
    let sql = format!("SELECT COUNT(*) FROM messages WHERE id = '{id}' AND body IS NULL");
    until(10, "the text is forgotten", || {
        common::count(&db, &sql) == 1
    });
    ok(&sock, &token, "stop", json!({}));
    exits_zero(child);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_writes_the_snapshot_outside_the_home() {
    let dir = tempfile::tempdir().unwrap();
    let (child, token, sock) = member(dir.path(), friday());
    assert_eq!(ok(&sock, &token, "stop", json!({})), json!({}));
    exits_zero(child);
    let snap = Store::open(&dir.path().join("last.db")).unwrap();
    assert!(!roster::entries(&snap).unwrap().is_empty());
    assert!(
        !dir.path().join("sock").exists(),
        "the socket file is removed at stop"
    );
}

// The test plays A's computer closing our link as SUPERSEDED, as it does when a second
// connection from this member's key replaces it (accepted by ruling over dialing twice).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_connection_with_this_key_emits_superseded() {
    let dir = tempfile::tempdir().unwrap();
    let a = seed(dir.path());
    let (child, token, sock) = member(dir.path(), friday());
    let mut sub = UnixStream::connect(&sock).unwrap();
    writeln!(
        sub,
        "{}",
        json!({"token": token, "id": 1, "op": "subscribe"})
    )
    .unwrap();
    sub.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut events = BufReader::new(sub);
    let mut first = String::new();
    events.read_line(&mut first).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&first).unwrap()["ok"],
        true,
        "{first}"
    );
    // A's computer took a second connection from this member's key (another process holding
    // it): it closes ours as superseded.
    let (pa, _ep) = dial(&sock, &token, &a).await;
    until(10, "A is online", || {
        ok(&sock, &token, "status", json!({}))["peers"]["online"] == 1
    });
    pa.conn.close(VarInt::from_u32(SUPERSEDED), b"superseded");
    let short = a.short.clone();
    let seen = tokio::task::spawn_blocking(move || loop {
        let mut l = String::new();
        if events.read_line(&mut l).unwrap() == 0 {
            return None;
        }
        let e: Value = serde_json::from_str(&l).unwrap();
        if e["event"] == "superseded" {
            return Some(e);
        }
    })
    .await
    .unwrap()
    .expect("a superseded event");
    assert_eq!(seen["by_peer"], short.as_str(), "{seen}");
    ok(&sock, &token, "stop", json!({}));
    exits_zero(child);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivered_record_left_unread_past_its_ttl_expires() {
    let dir = tempfile::tempdir().unwrap();
    let a = seed(dir.path());
    let (child, token, sock) = member(dir.path(), friday());
    let (mut pa, _ep) = dial(&sock, &token, &a).await;
    let to = format!("{}/friday", short(&sock, &token));
    let (id, _) = send(&mut pa, &a, &to, "a turn that always fails").await;
    let ack = json!({"session": "friday", "record": id, "stage": "delivered"});
    ok(&sock, &token, "inbox.ack", ack);
    ok(&sock, &token, "stop", json!({}));
    exits_zero(child);
    // Delivered more than `outbox::TTL_S` ago, never read.
    let db = dir.path().join("home/state.db");
    let c = rusqlite::Connection::open(&db).unwrap();
    let old = format!("updated_at - {}", lightbulb::outbox::TTL_S + 10);
    c.execute(
        &format!("UPDATE receipts SET updated_at = {old} WHERE state = 'delivered'"),
        [],
    )
    .unwrap();
    drop(c);
    let (child, token, sock) = member(dir.path(), friday());
    let peek = ok(&sock, &token, "inbox.peek", json!({}));
    assert_eq!(peek["sessions"], json!([]), "{peek}");
    let sql = format!(
        "SELECT COUNT(*) FROM receipts r JOIN messages m ON m.from_key = r.from_key \
         AND m.request_id = r.request_id WHERE m.id = '{id}' AND r.state = 'expired' \
         AND m.body IS NULL"
    );
    until(10, "the receipt reads expired", || {
        common::count(&db, &sql) == 1
    });
    ok(&sock, &token, "stop", json!({}));
    exits_zero(child);
}
