//! Presence and the directory across two computers: every member sees every session, a
//! presence that names another member's prefix is dropped, a name one computer keeps is
//! refused on the same person's other computer, and an `off` folder is never published.

use iroh::Endpoint;
use lightbulb::daemon::ipc;
use lightbulb::identity::Identity;
use lightbulb::net::frames::{Ended, Frame, Remote};
use lightbulb::os;
use lightbulb::roster::Entry;
use serde_json::{json, Value};

mod common;

use common::{endpoint, out, presence_with, remote, seed_pair, sees, start_pair, until};
use common::{Env, Peer, EXE};

/// `p` as `folder` writes it to `folders_off`: resolved, lowercased on macOS.
fn stored_off(p: &std::path::Path) -> String {
    let r = lightbulb::presence::resolved(p)
        .to_str()
        .unwrap()
        .to_string();
    if cfg!(target_os = "macos") {
        r.to_lowercase()
    } else {
        r
    }
}

/// This test process as a `provider` session `conversation` in `cwd`.
fn register(e: &Env, provider: &str, conversation: &str, cwd: &str) -> Value {
    let pid = std::process::id();
    let birth = os::process(pid).unwrap().birth_micros;
    e.ask(
        json!({"id": 1, "op": "register", "session": {"provider": provider,
        "conversation": conversation, "pid": pid, "birth_micros": birth, "cwd": cwd}}),
    )
}

fn directory(e: &Env, query: Option<&str>) -> Vec<Value> {
    let r = e.ask(json!({"id": 1, "op": "directory", "query": query}));
    assert_eq!(r["ok"], true, "{r}");
    r["sessions"].as_array().unwrap().clone()
}

fn ids(records: &[Value]) -> Vec<String> {
    let mut v: Vec<String> = records
        .iter()
        .map(|r| r["session_id"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_member_sees_the_whole_directory() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    let (sa, sb) = (a.short(), b.short());
    start_pair(&a, &b, "", &[]);
    assert_eq!(register(&a, "codex", "a1", "/tmp/work/api")["ok"], true);
    assert_eq!(register(&b, "claude", "b1", "/tmp/work/web")["ok"], true);
    let (ka, kb) = (format!("{sa}/codex:a1"), format!("{sb}/claude:b1"));
    let both = {
        let mut v = vec![ka.clone(), kb.clone()];
        v.sort();
        v
    };
    until(10, "each sees both sessions", || {
        ids(&directory(&a, None)) == both && ids(&directory(&b, None)) == both
    });

    // A's view: its own session with its full folder, B's with none, and B's person.
    let on_a = directory(&a, None);
    let mine = on_a
        .iter()
        .find(|r| r["session_id"] == ka.as_str())
        .unwrap();
    let theirs = on_a
        .iter()
        .find(|r| r["session_id"] == kb.as_str())
        .unwrap();
    assert_eq!(
        (&mine["cwd"], &mine["person"], &mine["computer"]),
        (&json!("/tmp/work/api"), &json!("prasiddha"), &json!("mac"))
    );
    for r in &on_a {
        assert!(
            r.get("can_send").is_none() && r.get("needs_owner_ok").is_none(),
            "{r}"
        );
    }
    for (field, want) in [
        ("cwd", json!("")),
        ("person", json!("bob")),
        ("person_verified", json!(false)),
        ("computer", json!("mbp")),
        ("agent", json!("claude")),
        ("status", json!("idle")),
        ("folder", json!("web")),
        ("wakes", json!(false)),
        ("online", json!(true)),
    ] {
        assert_eq!(theirs[field], want, "{field}: {theirs}");
    }
    assert!(theirs.get("seen_at").is_none(), "{theirs}");
    assert_eq!(mine["folder"], "api", "{mine}");
    assert!(
        mine["pid"].is_u64() && mine["birth_micros"].is_u64(),
        "{mine}"
    );
    // The query matches person, computer, agent, repo, folder and session id, any case.
    // (A random member key may itself contain the query: only such an id may join.)
    let only = |q: &str, want: &str| {
        let got = ids(&directory(&a, Some(q)));
        assert!(got.iter().any(|g| g == want), "{q}: {got:?}");
        let q = q.to_lowercase();
        assert!(
            got.iter().all(|g| g == want || g.contains(&q)),
            "{q}: {got:?}"
        );
    };
    only("MBP", &kb);
    only("api", &ka);
    assert!(directory(&a, Some("nothing-matches")).is_empty());

    // A change on B reaches A: a new session, then an ended one.
    assert_eq!(register(&b, "codex", "b2", "/tmp/work/cli")["ok"], true);
    let kb2 = format!("{sb}/codex:b2");
    until(10, "A sees B's new session", || sees(&a, &kb2));
    let r = b.ask(json!({"id": 1, "op": "end", "key": kb}));
    assert_eq!(r["ended"], true, "{r}");
    until(10, "A sees B's session end", || !sees(&a, &kb));

    // `team.get` says who is online; `who --all` lists B's sessions after A's.
    let members = a.ask(json!({"id": 1, "op": "team.get"}))["members"].clone();
    let bob = members
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "bob");
    assert_eq!(bob.unwrap()["online"], true, "{members}");
    let o = a.detached(&[EXE, "who", "--all"], "");
    assert!(o.status.success(), "{}", common::err(&o));
    // By handle, ids only in `--json`.
    let text = out(&o);
    // This computer's by handle; a teammate's by the address `send` takes.
    assert!(!text.contains(&ka), "{text}");
    assert!(text.lines().any(|l| l.starts_with(&kb2)), "{text}");
    let rows: Vec<&str> = text.lines().skip(1).collect();
    assert!(
        rows[0].contains("prasiddha") && rows[0].contains("mac"),
        "{text}"
    );
    assert!(
        rows[1..]
            .iter()
            .any(|l| l.contains("bob") && l.contains("mbp")),
        "{text}"
    );
    // Handles are this computer's sessions' alone: a teammate's takes no number.
    assert!(rows[0].starts_with("codex-1 "), "{text}");
    let d = a.ask(json!({"id": 1, "op": "directory"}));
    let theirs = d["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["session_id"] == kb2.as_str());
    assert!(theirs.into_iter().all(|s| s["handle"].is_null()), "{d}");

    // B's computer stops: its session stays listed, offline since then.
    assert_eq!(b.ask(json!({"id": 1, "op": "shutdown"}))["ok"], true);
    let row = || {
        directory(&a, None)
            .into_iter()
            .find(|r| r["session_id"] == kb2.as_str())
            .unwrap()
    };
    until(15, "A sees B offline", || row()["online"] == false);
    let seen_at = row()["seen_at"].as_i64().unwrap();
    assert!((common::now_s() - seen_at).abs() < 60, "{seen_at}");
    assert!(directory(&a, None)
        .iter()
        .find(|r| r["session_id"] == ka.as_str())
        .unwrap()
        .get("seen_at")
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_record_says_who_its_proof_claims() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    a.sign_in("user_aden", None);
    start_pair(&a, &b, "", &[]);
    assert_eq!(register(&a, "codex", "a1", "/tmp/work/api")["ok"], true);
    assert_eq!(register(&b, "codex", "b1", "/tmp/work/web")["ok"], true);
    let (ka, kb) = (
        format!("{}/codex:a1", a.short()),
        format!("{}/codex:b1", b.short()),
    );
    until(10, "each sees both", || sees(&a, &kb) && sees(&b, &ka));
    let of = |e: &Env, key: &str| {
        directory(e, None)
            .into_iter()
            .find(|r| r["session_id"] == key)
            .unwrap()
    };
    assert_eq!(of(&a, &ka)["user_id"], "user_aden");
    assert_eq!(of(&b, &ka)["user_id"], "user_aden");
    assert!(of(&a, &kb).get("user_id").is_none());
    assert!(of(&b, &kb).get("user_id").is_none());
}

/// A bad row costs only itself: not its sessions, not the other ended rows, whatever else the
/// sender sent. A reason this build does not know reads as given.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_bad_ended_row_costs_only_itself() {
    let (a, b) = (Env::new(), Env::new());
    let [_, admit] = seed_pair(&a, &b);
    a.daemon(false, "", &[]);
    let ib = b.identity();
    let (_ep, mut p) = played(&ib, &a, &admit).await;
    until(5, "A sees B online", || a.status()["peers"]["online"] == 1);
    let sb = ib.short.clone();
    let row = |c: &str, reason: &str, ended_at: u64| Ended {
        session_id: format!("{sb}/claude:{c}"),
        agent: "claude".into(),
        folder: "web".into(),
        title: None,
        repo: None,
        branch: None,
        ended_at,
        reason: reason.into(),
    };
    let now = ipc::now();
    let id = format!("{sb}/codex:live");
    let mut f = presence(&ib.key, 1, &[&id]);
    if let Frame::Presence { ended, .. } = &mut f {
        *ended = vec![
            row("good", "session-end", now),
            row("bad-reason", "NOT A WORD", now),
            row("ahead", "session-end", now + 2 * 86_400),
            row("new-word", "some-new-reason", now),
        ];
    }
    p.send(&f).await;
    let of = |c: &str| {
        let key = format!("{sb}/claude:{c}");
        directory(&a, None)
            .into_iter()
            .find(|r| r["session_id"] == key.as_str())
    };
    until(10, "A takes the presence", || sees(&a, &id));
    assert_eq!(of("good").unwrap()["status"], "ended");
    assert_eq!(of("new-word").unwrap()["reason"], "some-new-reason");
    assert!(of("bad-reason").is_none());
    assert!(of("ahead").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_teammates_ended_session_is_seen_for_a_week_and_never_its_resume() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    start_pair(&a, &b, "", &[]);
    assert_eq!(register(&a, "claude", "a1", "/tmp/work/api")["ok"], true);
    let key = format!("{}/claude:a1", a.short());
    until(10, "B sees the session", || sees(&b, &key));
    let r = a.ask(json!({"id": 1, "op": "end", "key": key, "reason": "session-end"}));
    assert_eq!(r["ended"], true, "{r}");

    let row = |e: &Env, q: Option<&str>| {
        directory(e, q)
            .into_iter()
            .find(|r| r["session_id"] == key.as_str())
    };
    until(10, "B sees it ended", || {
        row(&b, None).is_some_and(|r| r["status"] == "ended")
    });
    let theirs = row(&b, None).unwrap();
    assert_eq!(theirs["reason"], "session-end", "{theirs}");
    assert!(theirs["ended_at"].is_u64(), "{theirs}");
    assert_eq!(theirs["cwd"], "", "{theirs}");
    assert!(theirs.get("resume").is_none(), "{theirs}");
    // A query finds an ended row too.
    assert!(row(&b, Some("prasiddha")).is_some());
    assert!(row(&b, Some("nothing-matches")).is_none());

    let mine = row(&a, None).unwrap();
    assert_eq!(mine["status"], "ended", "{mine}");
    assert_eq!(
        mine["resume"], "cd '/tmp/work/api' && claude --resume a1",
        "{mine}"
    );
    assert_eq!(mine["cwd"], "/tmp/work/api", "{mine}");

    // The same conversation live again is one live row, on both sides.
    assert_eq!(register(&a, "claude", "a1", "/tmp/work/api")["ok"], true);
    until(10, "B sees it live again", || sees(&b, &key));
    let all = directory(&a, None);
    let rows = all.iter().filter(|r| r["session_id"] == key.as_str());
    assert_eq!(rows.count(), 1, "{all:?}");
    let all = directory(&b, None);
    let rows = all.iter().filter(|r| r["session_id"] == key.as_str());
    assert_eq!(rows.count(), 1, "{all:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_presence_with_fifty_one_ended_rows_is_dropped() {
    let (a, b) = (Env::new(), Env::new());
    let [_, admit] = seed_pair(&a, &b);
    a.daemon(false, "", &[]);
    let ib = b.identity();
    let (_ep, mut p) = played(&ib, &a, &admit).await;
    until(5, "A sees B online", || a.status()["peers"]["online"] == 1);
    let live = format!("{}/codex:live", ib.short);
    let ended = |n: usize| -> Vec<Ended> {
        (0..n)
            .map(|i| Ended {
                session_id: format!("{}/codex:e{i}", ib.short),
                agent: "codex".into(),
                folder: "x".into(),
                title: None,
                repo: None,
                branch: None,
                ended_at: ipc::now(),
                reason: "session-end".into(),
            })
            .collect()
    };
    let with = |seq: u64, n: usize| {
        let mut f = presence(&ib.key, seq, &[&live]);
        if let Frame::Presence { ended: e, .. } = &mut f {
            *e = ended(n);
        }
        f
    };
    p.send(&with(1, 50)).await;
    let fifty = || {
        directory(&a, None)
            .iter()
            .filter(|r| r["status"] == "ended")
            .count()
    };
    until(5, "A holds the fifty", || fifty() == 50);
    // The next one is past the cap: all of it goes, and the fence after it shows A read on.
    p.send(&with(2, 51)).await;
    let fence = format!("{}/codex:fence", ib.short);
    p.send(&presence(&ib.key, 3, &[&fence])).await;
    until(5, "A applies the fence", || sees(&a, &fence));
    let log = std::fs::read_to_string(a.state.path().join("daemon.log")).unwrap();
    let line = format!("net: dropped a presence frame from {}", ib.short);
    assert_eq!(log.matches(&line).count(), 1, "{log}");
}

/// `id` played over its own endpoint: dialed into `env` and past its `hello` (at `head`).
async fn played(id: &Identity, env: &Env, head: &Entry) -> (Endpoint, Peer) {
    let ep = endpoint(id).await;
    let mut p = Peer::dial(&ep, env).await;
    p.hello(vec![head.hash()]).await;
    (ep, p)
}

/// `member`'s presence `seq` of the sessions `ids`.
fn presence(member: &str, seq: u64, ids: &[&str]) -> Frame {
    presence_with(member, seq, ids.iter().map(|s| remote(s)).collect())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn presence_naming_another_prefix_is_dropped() {
    let (a, b) = (Env::new(), Env::new());
    let [_, admit] = seed_pair(&a, &b);
    a.daemon(false, "", &[]);
    let (ia, ib) = (a.identity(), b.identity());
    let (sa, sb) = (ia.short.clone(), ib.short.clone());
    let (_ep, mut p) = played(&ib, &a, &admit).await;
    until(5, "A sees B online", || a.status()["peers"]["online"] == 1);

    let good = format!("{sb}/codex:ok");
    p.send(&presence(&ib.key, 1, &[&good])).await;
    until(5, "A stores B's presence", || sees(&a, &good));
    // Higher seqs, so only the gate can keep them out: A's key as `member`, a session or a
    // kept name under A's prefix, then a fence under B's own.
    let forged = format!("{sa}/codex:forged");
    p.send(&presence(&ia.key, 2, &[&good, &forged])).await;
    p.send(&presence(&ib.key, 3, &[&good, &forged])).await;
    let mut kept_forged = presence(&ib.key, 4, &[&good]);
    if let Frame::Presence { kept, .. } = &mut kept_forged {
        kept.push(format!("{sa}/desk"));
    }
    p.send(&kept_forged).await;
    // Malformed sessions: text past the cap, an agent other than the id's provider, an unknown
    // status; and a seq more than a day ahead.
    let bad = |f: fn(&mut Remote)| {
        let mut r = remote(&good);
        f(&mut r);
        presence_with(&ib.key, 4, vec![r])
    };
    p.send(&bad(|r| r.doing = Some("x".repeat(257)))).await;
    p.send(&bad(|r| r.agent = "claude".into())).await;
    p.send(&bad(|r| r.status = "gone".into())).await;
    let ahead = ipc::now_ms() + 2 * 86_400_000;
    p.send(&presence(&ib.key, ahead, &[&good])).await;
    let fence = format!("{sb}/codex:fence");
    p.send(&presence(&ib.key, 5, &[&fence])).await;
    until(5, "A applies the fence", || sees(&a, &fence));
    assert!(!sees(&a, &forged));
    // An older presence is ignored.
    p.send(&presence(&ib.key, 4, &[&good])).await;
    let late = format!("{sb}/codex:late");
    p.send(&presence(&ib.key, 6, &[&fence, &late])).await;
    until(5, "A applies the next one", || sees(&a, &late));
    assert!(!sees(&a, &good));

    let log = std::fs::read_to_string(a.state.path().join("daemon.log")).unwrap();
    let line = format!("net: dropped a presence frame from {sb}");
    assert_eq!(log.matches(&line).count(), 7, "{log}");

    // A teammate's text reaches the person's terminal without what a terminal acts on.
    let mut r = remote(&late);
    r.doing = Some("\u{1b}]0;pwned\u{7}red\u{1b}[31m\u{202e}txt".into());
    p.send(&presence_with(&ib.key, 7, vec![r])).await;
    until(5, "A applies it", || {
        ids(&directory(&a, None)) == [late.clone()]
    });
    let o = a.detached(&[EXE, "who", "--all"], "");
    let text = out(&o);
    assert!(text.contains("]0;pwnedred[31mtxt"), "{text}");
    assert!(!text.contains(['\u{1b}', '\u{7}', '\u{202e}']), "{text:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn off_folder_sessions_are_not_published() {
    let (a, b) = (Env::new(), Env::new());
    let [_, admit] = seed_pair(&a, &b);
    let root = tempfile::tempdir().unwrap();
    let (secret, open) = (root.path().join("secret"), root.path().join("open"));
    let hidden = secret.join("proj");
    for d in [&hidden, &open, &root.path().join("x")] {
        std::fs::create_dir_all(d).unwrap();
    }
    // The off folder as `folder` stores it, its sessions as their hook reports them: both
    // resolved (the daemon itself makes no filesystem call on either).
    std::os::unix::fs::symlink(&secret, root.path().join("alias")).unwrap();
    let off = root.path().join("x/../secret");
    a.daemon(false, "", &[]);
    let sa = a.short();
    let (open, hidden) = (
        open.to_str().unwrap().to_string(),
        lightbulb::presence::resolved(&root.path().join("alias/proj"))
            .to_str()
            .unwrap()
            .to_string(),
    );
    for (conversation, cwd) in [
        ("o1", &open),
        ("o2", &open),
        ("h1", &hidden),
        ("h2", &hidden),
    ] {
        assert_eq!(register(&a, "codex", conversation, cwd)["ok"], true);
    }
    // Ended with mail waiting: the open one is `known`, the off-folder one nowhere.
    for c in ["o2", "h1"] {
        let to = format!("{sa}/codex:{c}");
        let r = a.ask(json!({"id": 1, "op": "send", "from": format!("{sa}/desk"),
            "to": to, "text": "x", "request_id": format!("r-{c}")}));
        assert_eq!(r["receipt"]["state"], "stored", "{r}");
        let r = a.ask(json!({"id": 1, "op": "end", "key": to}));
        assert_eq!(r["ended"], true, "{r}");
    }
    // The folder is turned off after its sessions registered (`reload`); a new one is ignored.
    let config = format!("lan = false\nfolders_off = [{:?}]\n", stored_off(&off));
    std::fs::write(a.state.path().join("config.toml"), config).unwrap();
    assert_eq!(a.ask(json!({"id": 1, "op": "reload"}))["ok"], true);
    assert_eq!(register(&a, "codex", "h3", &hidden)["ignored"], true);

    let (_ep, mut p) = played(&b.identity(), &a, &admit).await;
    let Frame::Presence {
        member,
        sessions,
        known,
        kept,
        ended,
        ..
    } = p.presence().await
    else {
        unreachable!()
    };
    assert_eq!(member, a.identity().key);
    let ids: Vec<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
    assert_eq!(ids, [format!("{sa}/codex:o1")]);
    // The last path part, not the path.
    assert_eq!(sessions[0].folder, "open");
    let o2 = format!("{sa}/codex:o2");
    assert_eq!((known, kept), (vec![o2.clone()], vec![o2.clone()]));
    // The off-folder session that ended is not on the wire: the open one is, by its last part.
    let ended_ids: Vec<&str> = ended.iter().map(|e| e.session_id.as_str()).collect();
    assert_eq!(ended_ids, [o2.as_str()]);
    assert_eq!(ended[0].folder, "open");
    // Locally the off-folder sessions are still listed, marked hidden, what ended there too: its
    // owner can still resume it.
    let local = directory(&a, None);
    assert_eq!(local.len(), 4, "{local:?}");
    let was = |s: &Value| s["status"] == "ended";
    let local_ended: Vec<&Value> = local.iter().filter(|s| was(s)).collect();
    assert_eq!(local_ended.len(), 2, "{local:?}");
    let h1 = format!("{sa}/codex:h1");
    let ended_row = |id: &str| {
        let row = local_ended.iter().find(|s| s["session_id"] == id);
        row.unwrap_or_else(|| panic!("{id}: {local:?}"))
    };
    assert_eq!(ended_row(&o2)["hidden"], Value::Null);
    assert_eq!(ended_row(&h1)["hidden"], true);
    assert!(ended_row(&h1)["resume"].is_string());
    let flag = |c: &str| {
        let id = format!("{sa}/codex:{c}");
        local
            .iter()
            .find(|s| s["session_id"] == id.as_str())
            .unwrap()["hidden"]
            .clone()
    };
    assert_eq!((flag("h2"), flag("o1")), (json!(true), Value::Null));
}

/// A headless run is on no one's list and never in `ended`; its address alone is published.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_headless_run_is_listed_nowhere() {
    let (a, b) = (Env::new(), Env::new());
    let [_, admit] = seed_pair(&a, &b);
    a.daemon(false, "", &[]);
    let p = os::process(std::process::id()).unwrap();
    let r = a.ask(
        json!({"id": 1, "op": "register", "session": {"provider": "claude",
        "conversation": "run", "pid": p.pid, "birth_micros": p.birth_micros, "cwd": "/tmp/p",
        "headless": true}}),
    );
    let run = r["key"].as_str().unwrap().to_string();
    assert_eq!(directory(&a, None), Vec::<Value>::new());
    let (_ep, mut peer) = played(&b.identity(), &a, &admit).await;
    let Frame::Presence {
        sessions,
        kept,
        ended,
        ..
    } = peer.presence().await
    else {
        unreachable!()
    };
    assert!(sessions.is_empty() && ended.is_empty(), "{sessions:?}");
    assert_eq!(kept, std::slice::from_ref(&run));

    let r = a.ask(json!({"id": 1, "op": "end", "key": run, "reason": "session-end"}));
    assert_eq!(r["ended"], true, "{r}");
    assert_eq!(directory(&a, None), Vec::<Value>::new());
    let db = a.state.path().join("state.db");
    assert_eq!(common::count(&db, "SELECT count(*) FROM sessions"), 0);
}

/// Months of finished jobs reach their owner's Recent, not teammates: the first read of Claude's
/// listing stamps the line. A job that ends after it is news.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_jobs_over_after_the_first_listing_are_published() {
    use std::os::unix::fs::PermissionsExt;
    let shim = tempfile::tempdir().unwrap();
    let listing = shim.path().join("listing.json");
    let job = |id: &str, started: u64| {
        json!({"cwd": "/tmp/p", "kind": "background", "id": "ab12", "state": "done",
            "name": "the job", "startedAt": started * 1000, "sessionId": id})
    };
    let old = "a1b2c3d4-0000-4000-8000-00000000000a";
    let new = "a1b2c3d4-0000-4000-8000-00000000000b";
    std::fs::write(&listing, json!([job(old, ipc::now() - 3_600)]).to_string()).unwrap();
    let exe = shim.path().join("claude");
    std::fs::write(&exe, format!("#!/bin/sh\ncat '{}'\n", listing.display())).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let env = [
        ("PATH", format!("{}:/usr/bin:/bin", shim.path().display())),
        ("LIGHTBULB_CONNECT_DEBUG_LISTING", "1".to_string()),
    ];
    let (a, b) = common::pair(&env);
    let sa = a.short();
    let (old, new) = (format!("{sa}/claude:{old}"), format!("{sa}/claude:{new}"));
    let row = |e: &Env, key: &str| {
        directory(e, None)
            .into_iter()
            .find(|r| r["session_id"] == key)
    };
    until(15, "its owner sees the old job", || row(&a, &old).is_some());
    assert_eq!(row(&a, &old).unwrap()["status"], "ended");

    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (first, second) = (
        job("a1b2c3d4-0000-4000-8000-00000000000a", ipc::now() - 3_600),
        job("a1b2c3d4-0000-4000-8000-00000000000b", ipc::now()),
    );
    std::fs::write(&listing, json!([first, second]).to_string()).unwrap();
    // A new row: news, so the listing is read soon.
    assert_eq!(register(&a, "codex", "poke", "/tmp/p")["ok"], true);
    until(15, "B sees the new job", || row(&b, &new).is_some());
    assert!(row(&b, &old).is_none(), "old news stays home");
    assert!(row(&a, &old).is_some());
}

/// A Claude session its hook registered under an off folder, which Claude's own file names
/// through a symlink: the hook's resolved folder wins over the discovery's, so it is hidden.
#[test]
fn a_discovered_claude_session_keeps_its_hooks_folder() {
    let (a, b) = common::pair(&[]);
    let root = tempfile::tempdir().unwrap();
    let secret = root.path().join("secret");
    std::fs::create_dir_all(secret.join("proj")).unwrap();
    let link = root.path().join("via-link");
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    let me = std::process::id();
    let claude = a.home.path().join(".claude/sessions");
    std::fs::create_dir_all(&claude).unwrap();
    let raw = link.join("proj");
    std::fs::write(
        claude.join(format!("{me}.json")),
        json!({"pid": me, "sessionId": "s1", "cwd": raw, "startedAt": lightbulb::os::started_ms(me).unwrap(),
            "status": "idle"})
        .to_string(),
    )
    .unwrap();
    let resolved = lightbulb::presence::resolved(&raw);
    let r = register(&a, "claude", "s1", resolved.to_str().unwrap());
    let key = r["key"]
        .as_str()
        .unwrap_or_else(|| panic!("{r}"))
        .to_string();
    until(15, "B sees the session", || sees(&b, &key));
    let config = std::fs::read_to_string(a.state.path().join("config.toml")).unwrap();
    let config = format!("{config}folders_off = [{:?}]\n", stored_off(&secret));
    std::fs::write(a.state.path().join("config.toml"), config).unwrap();
    assert_eq!(a.ask(json!({"id": 1, "op": "reload"}))["ok"], true);
    until(15, "B no longer sees it", || !sees(&b, &key));
}

/// A Claude session teammates already see by discovery (Claude's own file names its folder
/// through a symlink) is refused by its first register once that folder is off: it has no row,
/// and its mark takes it (title too) from teammates at once, while this computer still lists
/// it. A register after the folder is shown again brings it back.
#[test]
fn a_session_refused_under_an_off_folder_is_not_discovered_either() {
    let (a, b) = common::pair(&[]);
    let root = tempfile::tempdir().unwrap();
    let secret = root.path().join("secret");
    std::fs::create_dir_all(secret.join("proj")).unwrap();
    let link = root.path().join("via-link");
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    let base = std::fs::read_to_string(a.state.path().join("config.toml")).unwrap();
    let set = |off: bool| {
        let extra = if off {
            format!("folders_off = [{:?}]\n", stored_off(&secret))
        } else {
            String::new()
        };
        std::fs::write(a.state.path().join("config.toml"), format!("{base}{extra}")).unwrap();
        assert_eq!(a.ask(json!({"id": 1, "op": "reload"}))["ok"], true);
    };
    let me = std::process::id();
    let claude = a.home.path().join(".claude/sessions");
    std::fs::create_dir_all(&claude).unwrap();
    let raw = link.join("proj");
    std::fs::write(
        claude.join(format!("{me}.json")),
        json!({"pid": me, "sessionId": "s2", "cwd": raw, "startedAt": lightbulb::os::started_ms(me).unwrap(),
            "status": "idle", "name": "secret plan"})
        .to_string(),
    )
    .unwrap();
    let key = format!("{}/claude:s2", a.short());
    until(15, "B sees the discovery", || sees(&b, &key));
    set(true);
    // The reload's own presence goes out first; the folder alone does not hide the discovery
    // (its cwd names the folder through the link).
    std::thread::sleep(std::time::Duration::from_millis(2500));
    assert!(sees(&b, &key));
    let resolved = lightbulb::presence::resolved(&raw);
    let r = register(&a, "claude", "s2", resolved.to_str().unwrap());
    assert_eq!(r["ignored"], true, "{r}");
    // At once (the register wakes the sender: under 2 s), not at a later 5 s tick or a restart.
    until(2, "B no longer sees it", || {
        let d = b.ask(json!({"id": 1, "op": "directory"})).to_string();
        !d.contains(&key) && !d.contains("secret plan")
    });
    let mine = a.ask(json!({"id": 1, "op": "sessions"}));
    assert!(
        mine.to_string().contains(&key),
        "this computer still lists it: {mine}"
    );
    let listed = |r: &Value| {
        r["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["key"] == key.as_str())
            .unwrap()["hidden"]
            .clone()
    };
    assert_eq!(listed(&mine), true, "{mine}");
    set(false);
    assert_eq!(
        register(&a, "claude", "s2", resolved.to_str().unwrap())["ok"],
        true
    );
    until(2, "B sees it again", || sees(&b, &key));
    let mine = a.ask(json!({"id": 1, "op": "sessions"}));
    assert_eq!(listed(&mine), Value::Null, "{mine}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_group_send_reaches_a_teammates_sessions() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    start_pair(&a, &b, "", &[]);
    assert_eq!(register(&b, "claude", "b1", "/tmp/work/web")["ok"], true);
    let kb = format!("{}/claude:b1", b.short());
    until(10, "A sees B's session", || {
        ids(&directory(&a, None)).contains(&kb)
    });
    // From A's terminal, `bob/*`: B's sessions as `who --all` sees them.
    let run = "exit(system(@ARGV) >> 8)";
    let argv = [
        "/usr/bin/perl",
        "-e",
        run,
        EXE,
        "send",
        "bob/*",
        "to the team",
    ];
    let o = a.detached(&argv, "");
    assert!(o.status.success(), "{}{}", out(&o), common::err(&o));
    assert!(out(&o).contains("(Claude Code)"), "{}", out(&o));
    assert_eq!(b.take_one(&kb)["text"], "to the team");

    // What has ended is listed, and a group send does not reach it.
    assert_eq!(
        b.ask(json!({"id": 1, "op": "end", "key": kb}))["ended"],
        true
    );
    until(10, "A sees it ended", || !sees(&a, &kb));
    let o = a.detached(&argv, "");
    assert!(!o.status.success(), "{}", out(&o));
    assert!(
        common::err(&o).contains("No live session matches."),
        "{}",
        common::err(&o)
    );
}

#[test]
fn a_worker_host_is_listed_by_its_teammate_with_free_slots() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    let key = format!("{}/claude:new", b.short());
    let config =
        "computer = \"mbp\"\nnetwork_workers = true\nworkers_offer = \"claude\"\nworkers_max = 2\n";
    b.daemon(false, config, &[]);
    // One worker running before A connects.
    b.store()
        .insert_worker(&lightbulb::store::Worker {
            id: 0,
            assignment: "as1".into(),
            session: format!("{}/claude:w1", b.short()),
            provider: "claude".into(),
            folder: "/tmp/w1".into(),
            pid: None,
            started_at: 1,
            ended_at: None,
            state: "running".into(),
            by: None,
        })
        .unwrap();
    a.daemon(false, "", &[common::peers(&[&b])]);
    let row = |e: &Env| {
        directory(e, None)
            .into_iter()
            .find(|r| r["session_id"] == key.as_str())
    };
    until(15, "A lists B's worker host", || row(&a).is_some());
    let r = row(&a).unwrap();
    assert_eq!(r["free"], 1, "{r}");
    assert_eq!(
        (&r["kind"], &r["agent"], &r["status"]),
        (&json!("worker"), &json!("claude"), &json!("idle")),
        "{r}"
    );
    assert_eq!(r["cwd"], "", "{r}");

    // `network_workers` turned off and reloaded: the row goes; back on, it comes back.
    let set = |k: &str, v: &str| {
        lightbulb::config::Config::set(b.state.path(), k, v).unwrap();
        assert_eq!(b.ask(json!({"id": 1, "op": "reload"}))["ok"], true);
    };
    set("network_workers", "false");
    until(15, "the host row is gone", || row(&a).is_none());
    set("network_workers", "true");
    until(15, "the host row is back", || row(&a).is_some());
    // `workers_offer` cleared and reloaded: the row goes.
    set("workers_offer", "");
    until(15, "the host row is gone", || row(&a).is_none());
}
