//! Presence and the directory across two computers: every member sees every session, a
//! presence that names another member's prefix is dropped, a name one computer keeps is
//! refused on the same person's other computer, and an `off` folder is never published.

use iroh::Endpoint;
use lightbulb_connect::daemon::{ipc, net_ops};
use lightbulb_connect::identity::Identity;
use lightbulb_connect::net::frames::{Frame, Remote};
use lightbulb_connect::os;
use lightbulb_connect::roster::{self, Entry, Kind};
use serde_json::{json, Value};

mod common;

use common::{endpoint, out, presence_with, remote, seed_pair, sees, start_pair, until};
use common::{Env, Peer, EXE};

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
    assert_eq!(mine["needs_owner_ok"], false);
    for (field, want) in [
        ("cwd", json!("")),
        ("person", json!("bob")),
        ("person_verified", json!(false)),
        ("computer", json!("mbp")),
        ("agent", json!("claude")),
        ("status", json!("idle")),
        ("can_send", json!(true)),
        ("needs_owner_ok", json!(true)),
    ] {
        assert_eq!(theirs[field], want, "{field}: {theirs}");
    }
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
    let text = out(&o);
    let (local, remote) = (text.find(&ka).unwrap(), text.find(&kb2).unwrap());
    assert!(local < remote, "{text}");
    let line = text.lines().find(|l| l.contains(&kb2)).unwrap();
    assert!(line.contains("bob") && line.contains("mbp"), "{text}");
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
async fn kept_name_is_refused_on_the_persons_other_computer() {
    // The same person on two computers: B's admit names A's person_id, with a proof.
    let (a, b) = (Env::new(), Env::new());
    let (ia, ib) = (a.identity(), b.identity());
    let g = net_ops::genesis(&ia, "prasiddha", "mac", "acme", 1);
    let admit = Entry::signed(
        &ia,
        vec![g.hash()],
        Kind::Admit,
        json!({"member": ib.key, "name": "prasiddha-mbp", "computer": "mbp", "person_id": ia.key,
               "kind": "computer", "role": "member", "proof": "test"}),
        2,
    );
    for env in [&a, &b] {
        let store = env.store();
        roster::accept(&store, &g, 10).unwrap();
        roster::accept(&store, &admit, 10).unwrap();
    }
    start_pair(&a, &b, "", &[]);

    // A ends a session and listens on a name: both are kept for a day.
    assert_eq!(register(&a, "codex", "c1", "/tmp/k")["ok"], true);
    let r = a.ask(json!({"id": 1, "op": "end", "key": format!("{}/codex:c1", ia.short)}));
    assert_eq!(r["ended"], true, "{r}");
    let r = a.ask(json!({"id": 1, "op": "inbox", "key": format!("{}/desk", ia.short)}));
    assert_eq!(r["ok"], true, "{r}");

    // A later session as a fence: once B lists it, B holds the presence that keeps both.
    assert_eq!(register(&a, "codex", "fence", "/tmp/k")["ok"], true);
    let fence = format!("{}/codex:fence", ia.short);
    until(10, "B has A's latest presence", || sees(&b, &fence));
    let r = register(&b, "codex", "c1", "/tmp/k");
    assert_eq!(r["error"], "name_taken", "{r}");
    let r = b.ask(json!({"id": 1, "op": "inbox", "key": format!("{}/desk", ib.short)}));
    assert_eq!(r["error"], "name_taken", "{r}");
    // Names A does not keep stay free, and A may use its own again.
    assert_eq!(register(&b, "codex", "c2", "/tmp/k")["ok"], true);
    let r = b.ask(json!({"id": 1, "op": "inbox", "key": format!("{}/lab", ib.short)}));
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(register(&a, "codex", "c1", "/tmp/k")["ok"], true);
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
    // The off folder spelled through `..`, its sessions through a symlink: both resolve.
    std::os::unix::fs::symlink(&secret, root.path().join("alias")).unwrap();
    let off = root.path().join("x/../secret");
    let config = format!("folders_off = [{:?}]\n", off.to_str().unwrap());
    a.daemon(false, &config, &[]);
    let sa = a.short();
    let (open, hidden) = (
        open.to_str().unwrap().to_string(),
        root.path().join("alias/proj").to_str().unwrap().to_string(),
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

    let (_ep, mut p) = played(&b.identity(), &a, &admit).await;
    let Frame::Presence {
        member,
        sessions,
        known,
        kept,
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
    assert_eq!((known, kept), (vec![o2.clone()], vec![o2]));
    // Locally the off-folder session is still listed.
    assert_eq!(directory(&a, None).len(), 2);
}
