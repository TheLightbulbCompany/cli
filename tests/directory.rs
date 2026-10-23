//! Presence and the directory across two computers: every member sees every session, a
//! presence that names another member's prefix is dropped, a name one computer keeps is
//! refused on the same person's other computer, and an `off` folder is never published.

use iroh::endpoint::{presets, RecvStream, SendStream};
use iroh::{Endpoint, SecretKey};
use lightbulb_network::daemon::net_ops;
use lightbulb_network::identity::Identity;
use lightbulb_network::net::frames::{self, Frame, Remote};
use lightbulb_network::net::{ALPN, PROTOCOL};
use lightbulb_network::os;
use lightbulb_network::roster::{self, Entry, Kind};
use serde_json::{json, Value};
use std::time::Duration;

mod common;

use common::{addr, out, peers, seed_pair, until, Env, EXE};

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

fn short(e: &Env) -> String {
    e.identity().short
}

/// Two running daemons of `seed`ed stores, A dialing B; returns once both are online.
fn pair(a: &Env, b: &Env, a_config: &str) {
    b.daemon(false, "", &[]);
    a.daemon(false, a_config, &[peers(&[b])]);
    let connected = json!({"online": 1, "total": 1});
    until(10, "both online", || {
        a.status()["peers"] == connected && b.status()["peers"] == connected
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_member_sees_the_whole_directory() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    let (sa, sb) = (short(&a), short(&b));
    pair(&a, &b, "");
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
    assert_eq!(ids(&directory(&a, Some("MBP"))), vec![kb.clone()]);
    assert_eq!(ids(&directory(&a, Some("api"))), vec![ka.clone()]);
    assert!(directory(&a, Some("nothing-matches")).is_empty());

    // A change on B reaches A: a new session, then an ended one.
    assert_eq!(register(&b, "codex", "b2", "/tmp/work/cli")["ok"], true);
    let kb2 = format!("{sb}/codex:b2");
    until(10, "A sees B's new session", || {
        ids(&directory(&a, None)).contains(&kb2)
    });
    let r = b.ask(json!({"id": 1, "op": "end", "key": kb}));
    assert_eq!(r["ended"], true, "{r}");
    until(10, "A sees B's session end", || {
        !ids(&directory(&a, None)).contains(&kb)
    });

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

/// A member played by the test over its own endpoint, holding `id`'s key.
struct Peer {
    send: SendStream,
    recv: RecvStream,
    _ep: Endpoint,
}

impl Peer {
    async fn dial(id: &Identity, env: &Env, head: &Entry) -> Peer {
        let ep = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::from_bytes(&id.signing.to_bytes()))
            .bind()
            .await
            .unwrap();
        let conn = ep.connect(addr(env), ALPN).await.unwrap();
        let (send, recv) = conn.open_bi().await.unwrap();
        let mut p = Peer {
            send,
            recv,
            _ep: ep,
        };
        let hello = Frame::Hello {
            protocol: PROTOCOL,
            heads: vec![head.hash()],
        };
        p.send(&hello).await;
        p
    }

    async fn send(&mut self, f: &Frame) {
        frames::write(&mut self.send, f).await.unwrap();
    }

    /// The first `presence` the daemon sends.
    async fn presence(&mut self) -> Frame {
        loop {
            let f = tokio::time::timeout(Duration::from_secs(10), frames::read(&mut self.recv))
                .await
                .expect("a frame within 10 s")
                .unwrap();
            if matches!(f, Frame::Presence { .. }) {
                return f;
            }
        }
    }
}

fn remote(session_id: &str) -> Remote {
    Remote {
        session_id: session_id.into(),
        agent: "codex".into(),
        repo: None,
        branch: None,
        folder: "x".into(),
        status: "idle".into(),
        doing: None,
        tier: "pull".into(),
        title: None,
    }
}

fn presence(member: &str, seq: u64, sessions: &[&str], kept: &[String]) -> Frame {
    Frame::Presence {
        member: member.into(),
        seq,
        sessions: sessions.iter().map(|s| remote(s)).collect(),
        known: vec![],
        kept: kept.to_vec(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn presence_naming_another_prefix_is_dropped() {
    let (a, b) = (Env::new(), Env::new());
    let [_, admit] = seed_pair(&a, &b);
    a.daemon(false, "", &[]);
    let (ia, ib) = (a.identity(), b.identity());
    let (sa, sb) = (ia.short.clone(), ib.short.clone());
    let mut p = Peer::dial(&ib, &a, &admit).await;
    until(5, "A sees B online", || a.status()["peers"]["online"] == 1);

    let good = format!("{sb}/codex:ok");
    p.send(&presence(&ib.key, 1, &[&good], &[])).await;
    until(5, "A stores B's presence", || {
        ids(&directory(&a, None)).contains(&good)
    });
    // Higher seqs, so only the gate can keep them out: A's key as `member`, a session or a
    // kept name under A's prefix, then a fence under B's own.
    let forged = format!("{sa}/codex:forged");
    p.send(&presence(&ia.key, 2, &[&good, &forged], &[])).await;
    p.send(&presence(&ib.key, 3, &[&good, &forged], &[])).await;
    p.send(&presence(&ib.key, 4, &[&good], &[format!("{sa}/desk")]))
        .await;
    let fence = format!("{sb}/codex:fence");
    p.send(&presence(&ib.key, 5, &[&fence], &[])).await;
    until(5, "A applies the fence", || {
        ids(&directory(&a, None)).contains(&fence)
    });
    assert!(!ids(&directory(&a, None)).contains(&forged));
    // An older presence is ignored.
    p.send(&presence(&ib.key, 4, &[&good], &[])).await;
    let late = format!("{sb}/codex:late");
    p.send(&presence(&ib.key, 6, &[&fence, &late], &[])).await;
    until(5, "A applies the next one", || {
        ids(&directory(&a, None)).contains(&late)
    });
    assert!(!ids(&directory(&a, None)).contains(&good));

    let log = std::fs::read_to_string(a.state.path().join("daemon.log")).unwrap();
    let line = format!("net: dropped a presence frame from {sb}");
    assert_eq!(log.matches(&line).count(), 3, "{log}");
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
    pair(&a, &b, "");

    // A ends a session and listens on a name: both are kept for a day.
    assert_eq!(register(&a, "codex", "c1", "/tmp/k")["ok"], true);
    let r = a.ask(json!({"id": 1, "op": "end", "key": format!("{}/codex:c1", ia.short)}));
    assert_eq!(r["ended"], true, "{r}");
    let r = a.ask(json!({"id": 1, "op": "inbox", "key": format!("{}/desk", ia.short)}));
    assert_eq!(r["ok"], true, "{r}");

    // A later session as a fence: once B lists it, B holds the presence that keeps both.
    assert_eq!(register(&a, "codex", "fence", "/tmp/k")["ok"], true);
    let fence = format!("{}/codex:fence", ia.short);
    until(10, "B has A's latest presence", || {
        ids(&directory(&b, None)).contains(&fence)
    });
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
    let secret = root.path().join("secret");
    let config = format!("folders_off = [{:?}]\n", secret.to_str().unwrap());
    a.daemon(false, &config, &[]);
    let sa = short(&a);
    let open = root.path().join("open");
    let hidden = secret.join("proj");
    assert_eq!(
        register(&a, "codex", "o1", open.to_str().unwrap())["ok"],
        true
    );
    assert_eq!(
        register(&a, "codex", "h1", hidden.to_str().unwrap())["ok"],
        true
    );
    // Ended under the off folder: not even its name is published.
    let r = a.ask(json!({"id": 1, "op": "end", "key": format!("{sa}/codex:h1")}));
    assert_eq!(r["ended"], true, "{r}");
    assert_eq!(
        register(&a, "codex", "h2", hidden.to_str().unwrap())["ok"],
        true
    );

    let mut p = Peer::dial(&b.identity(), &a, &admit).await;
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
    assert!(known.is_empty() && kept.is_empty(), "{known:?} {kept:?}");
    // Locally the off-folder session is still listed.
    assert_eq!(directory(&a, None).len(), 2);
}
