//! What the CLI and network test files share: a temp state dir and HOME, a daemon shut down
//! on drop, a person's shell (`detached`) and socket client (`person`), the daemon env knobs,
//! a two-member team seeded before start, and a member played by the test (`Peer`).
//! ponytail: only `cli.rs`, `team_cli.rs`, `net_two.rs`, `join.rs`, `ui_claim.rs`, `dialog.rs`, `signin.rs`,
//! `directory.rs`, `envelope.rs` and `perf.rs` use it;
//! the other test files keep their own copies.
#![allow(dead_code)]

pub mod issuer;

use iroh::endpoint::{presets, Connection, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, PublicKey, SecretKey};
use lightbulb::daemon::net_ops;
use lightbulb::identity::{self, Identity};
use lightbulb::net::frames::{self, Frame, Remote};
use lightbulb::net::{ALPN, PROTOCOL};
use lightbulb::policy;
use lightbulb::roster::{self, Entry, Kind};
use lightbulb::store::Store;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

pub const EXE: &str = env!("CARGO_BIN_EXE_lightbulb");

pub struct Env {
    pub state: tempfile::TempDir,
    pub home: tempfile::TempDir,
    /// `LIGHTBULB_CONNECT_HOOK_CALLER` for the daemon: the process it treats as an agent.
    pub pin: Option<String>,
}

impl Env {
    pub fn new() -> Env {
        Env {
            state: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
            pin: None,
        }
    }

    /// `c` with this env's state dir, Claude home, HOME and user, launchctl off.
    pub fn base_cmd(&self, mut c: Command) -> Command {
        c.env("LIGHTBULB_CONNECT_HOME", self.state.path())
            .env("LIGHTBULB_CONNECT_CLAUDE_HOME", self.home.path())
            .env("LIGHTBULB_CONNECT_NO_LAUNCHCTL", "1")
            .env("LIGHTBULB_CONNECT_DEBUG_NO_DEFAULT_RELAYS", "1")
            .env("HOME", self.home.path())
            .env("USER", "prasiddha");
        c
    }

    /// One daemon request on a fresh connection (the CLI starts the daemon on first use).
    pub fn ask(&self, body: Value) -> Value {
        let mut s = UnixStream::connect(self.state.path().join("sock")).unwrap();
        writeln!(s, "{body}").unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    pub fn identity(&self) -> Identity {
        Identity::load_or_create(self.state.path()).unwrap()
    }

    pub fn store(&self) -> Store {
        Store::open(&self.state.path().join("state.db")).unwrap()
    }

    /// Starts `lightbulb daemon` on this state dir with `lan` (mDNS) and `config` as
    /// `config.toml`, and `env` set, and waits until it answers.
    pub fn daemon(&self, lan: bool, config: &str, env: &[(&str, String)]) {
        let config = format!("lan = {lan}\n{config}");
        std::fs::write(self.state.path().join("config.toml"), config).unwrap();
        let log = std::fs::File::create(self.state.path().join("daemon.log")).unwrap();
        let mut c = self.base_cmd(Command::new(EXE));
        c.arg("daemon")
            // No real dialog on the screen of whoever runs the tests: unanswered, unless a test
            // asks for an answer (`dialog`) or a fake `osascript`.
            .env("LIGHTBULB_CONNECT_DIALOG", "")
            .env_remove("LIGHTBULB_CONNECT_PEERS")
            .env_remove("LIGHTBULB_CONNECT_DEBUG_PROTOCOL")
            .env_remove("LIGHTBULB_CONNECT_DEBUG_RELAY_KEYS")
            .env_remove("LIGHTBULB_CONNECT_DEBUG_RELAY_ONLY")
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null());
        if env
            .iter()
            .any(|(k, _)| *k == "LIGHTBULB_CONNECT_DEBUG_OSASCRIPT")
        {
            c.env_remove("LIGHTBULB_CONNECT_DIALOG");
        }
        c.stdout(log.try_clone().unwrap()).stderr(log);
        let mut child = c.spawn().unwrap();
        let ping = || {
            UnixStream::connect(self.state.path().join("sock")).is_ok_and(|mut s| {
                let mut line = String::new();
                writeln!(s, r#"{{"id":0,"op":"ping"}}"#).is_ok()
                    && BufReader::new(s).read_line(&mut line).is_ok()
                    && line.contains("\"ok\":true")
            })
        };
        until(10, "the daemon starts", || {
            assert!(child.try_wait().unwrap().is_none(), "the daemon exited");
            ping()
        });
        std::thread::spawn(move || child.wait());
    }

    /// This computer's member prefix.
    pub fn short(&self) -> String {
        self.identity().short
    }

    /// `send`'s receipt (the op must succeed).
    pub fn send(
        &self,
        from: &str,
        to: &str,
        text: &str,
        rid: &str,
        reply_to: Option<&str>,
    ) -> Value {
        let r = self.ask(
            json!({"id": 1, "op": "send", "from": from, "to": to, "text": text,
            "request_id": rid, "reply_to": reply_to}),
        );
        assert_eq!(r["ok"], true, "{r}");
        r["receipt"].clone()
    }

    pub fn receipt(&self, from: &str, rid: &str) -> Value {
        self.ask(json!({"id": 1, "op": "receipt", "from": from, "request_id": rid}))["receipt"]
            .clone()
    }

    /// The receipt's state, `""` when there is none.
    pub fn state(&self, from: &str, rid: &str) -> String {
        self.receipt(from, rid)["state"]
            .as_str()
            .unwrap_or_default()
            .into()
    }

    /// Takes what waits for `key`.
    pub fn inbox(&self, key: &str) -> Vec<Value> {
        let r = self.ask(json!({"id": 1, "op": "inbox", "key": key}));
        r["messages"].as_array().cloned().unwrap_or_default()
    }

    /// Waits up to 10 s for one message to `key` and takes it.
    pub fn take_one(&self, key: &str) -> Value {
        let mut got = vec![];
        until(10, "a message arrives", || {
            got = self.inbox(key);
            !got.is_empty()
        });
        assert_eq!(got.len(), 1, "{got:?}");
        got.remove(0)
    }

    pub fn unread(&self, key: &str) -> u64 {
        self.ask(json!({"id": 1, "op": "unread", "key": key}))["unread"]
            .as_u64()
            .unwrap()
    }

    pub fn status(&self) -> Value {
        self.ask(json!({"id": 1, "op": "status"}))
    }

    pub fn heads(&self) -> Value {
        self.ask(json!({"id": 1, "op": "team.get"}))["heads"].clone()
    }

    /// `<member key>@127.0.0.1:<port>`: this running daemon, for `LIGHTBULB_CONNECT_PEERS`.
    pub fn peer_arg(&self) -> String {
        format!(
            "{}@127.0.0.1:{}",
            self.identity().key,
            self.status()["port"]
        )
    }

    /// Runs `argv` reparented to launchd, so no agent is above it even when the tests run
    /// under one: the daemon sees a person, as at a terminal.
    pub fn detached(&self, argv: &[&str], stdin: &str) -> Output {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("in"), stdin).unwrap();
        let script =
            r#"( "$@" <"$D/in" >"$D/out" 2>"$D/err"; echo $? >"$D/c"; mv "$D/c" "$D/code" ) &"#;
        let mut c = self.base_cmd(Command::new("/bin/sh"));
        c.args(["-c", script, "sh"])
            .args(argv)
            .env("D", d.path())
            .env_remove("LIGHTBULB_CONNECT_HOOK_CALLER");
        if let Some(pin) = &self.pin {
            c.env("LIGHTBULB_CONNECT_HOOK_CALLER", pin);
        }
        assert!(c.status().unwrap().success());
        let deadline = Instant::now() + Duration::from_secs(30);
        let code = loop {
            if let Ok(c) = std::fs::read_to_string(d.path().join("code")) {
                break c.trim().parse::<i32>().unwrap();
            }
            assert!(Instant::now() < deadline, "{argv:?} did not finish");
            std::thread::sleep(Duration::from_millis(20));
        };
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: std::fs::read(d.path().join("out")).unwrap(),
            stderr: std::fs::read(d.path().join("err")).unwrap(),
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Ok(mut s) = UnixStream::connect(self.state.path().join("sock")) {
            let _ = writeln!(s, r#"{{"id":1,"op":"shutdown"}}"#);
            let _ = s.read(&mut [0; 64]);
        }
    }
}

/// `seed_team`, and each person already allows the other (`people` accept), so their mail is
/// not held.
pub fn seed_pair(a: &Env, b: &Env) -> [Entry; 2] {
    let seeded = seed_team(a, b);
    for (env, other) in [(a, b), (b, a)] {
        let key = other.identity().key;
        let accept = policy::Verdict::Accept;
        policy::set_person(&env.store(), &key, accept, "local", 1).unwrap();
    }
    seeded
}

/// `who` admitted by `by` after `parent` as `name` (its computer too), a computer member of
/// its own person, with `extra`'s fields over those.
pub fn admit(by: &Identity, parent: &Entry, who: &Identity, name: &str, extra: Value) -> Entry {
    let mut body = json!({"member": who.key, "name": name, "computer": name,
        "person_id": who.key, "kind": "computer", "role": "member"});
    for (k, v) in extra.as_object().expect("an object") {
        body[k] = v.clone();
    }
    Entry::signed(by, vec![parent.hash()], Kind::Admit, body, 2)
}

/// `who`'s computer proof (`issuer::computer_proof`), issued now: person `sub` called `name`,
/// signed into workspace `ws` (named for its id) if any.
pub fn person_proof(who: &Identity, sub: &str, name: &str, ws: Option<&str>) -> String {
    issuer::computer_proof(&who.key, sub, name, ws.map(|w| (w, w)), now_s())
}

/// `who`'s workspace proof (`issuer::workspace_proof`) for workspace `ws` called `name`, with
/// no agents, issued now.
pub fn ws_proof(who: &Identity, ws: &str, name: &str) -> String {
    issuer::workspace_proof(&who.key, ws, name, &[], now_s())
}

/// `LIGHTBULB_CONNECT_PEERS` is read from this process's environment when an instance starts:
/// one start at a time.
static STARTING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// An embedded instance on `home`, workspace `name`, that dials `envs`
/// at their running daemons, leaving a final snapshot at `snapshot` on shutdown; `knobs` are
/// set while it starts (its knobs are read then).
pub async fn instance(
    home: &std::path::Path,
    envs: &[&Env],
    name: &str,
    snapshot: Option<&std::path::Path>,
    knobs: &[(&str, String)],
) -> lightbulb::Result<lightbulb::embed::Instance> {
    let _one = STARTING.lock().await;
    let knobs: Vec<(&str, String)> = [peers(envs)]
        .into_iter()
        .chain(knobs.iter().cloned())
        .collect();
    for (k, v) in &knobs {
        std::env::set_var(k, v);
    }
    let opts = lightbulb::embed::EmbedOpts {
        kind: roster::MemberKind::Cloud,
        relay: None,
        snapshot: snapshot.map(std::path::Path::to_path_buf),
        name: name.into(),
    };
    let i = lightbulb::embed::start(home, opts).await;
    for (k, _) in &knobs {
        std::env::remove_var(k);
    }
    i
}

/// Instance `i`'s answer to `op` with `body`.
pub async fn call(i: &lightbulb::embed::Instance, op: &str, body: Value) -> Value {
    let mut req = body;
    req["id"] = 1.into();
    req["op"] = op.into();
    i.call(req).await
}

/// Waits (up to 10 s) until instance `i` has `n` members online.
pub async fn online(i: &lightbulb::embed::Instance, n: u64) {
    for _ in 0..500 {
        if call(i, "status", json!({})).await["peers"]["online"] == n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{n} online");
}

/// `who` played by the test, dialing instance `i` on the member ALPN, `hello` sent.
pub async fn dial_instance(i: &lightbulb::embed::Instance, who: &Identity) -> (Peer, Endpoint) {
    let key = call(i, "identity.get", json!({})).await["member_key"].clone();
    let port = call(i, "status", json!({})).await["port"].as_u64().unwrap();
    let key = PublicKey::from_bytes(&identity::key_bytes(key.as_str().unwrap()).unwrap()).unwrap();
    let addr = EndpointAddr::new(key).with_ip_addr(format!("127.0.0.1:{port}").parse().unwrap());
    let ep = endpoint(who).await;
    let conn = ep.connect(addr, ALPN).await.unwrap();
    let (send, recv) = conn.open_bi().await.unwrap();
    let mut p = Peer { conn, send, recv };
    p.hello(vec![]).await;
    (p, ep)
}

/// `<short>/codex:<conv>` on `e`.
pub fn session(e: &Env, conv: &str) -> String {
    format!("{}/codex:{conv}", e.short())
}

/// Unix seconds.
pub fn now_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// `e`'s invite link of `mode` (`team.invite`): its ticket.
pub fn invite(e: &Env, mode: &str) -> String {
    let r = e.ask(json!({"id": 1, "op": "team.invite", "mode": mode}));
    assert_eq!(r["ok"], true, "{r}");
    r["ticket"].as_str().unwrap().into()
}

/// `e`'s members (`team.get`).
pub fn members(e: &Env) -> Vec<Value> {
    let r = e.ask(json!({"id": 1, "op": "team.get"}));
    r["members"].as_array().unwrap().clone()
}

/// The same team in both stores before either daemon starts: `a` founds "acme" (computer
/// "mac") and admits `b` as "bob" (computer "mbp"). Returns `[genesis, admit]`.
pub fn seed_team(a: &Env, b: &Env) -> [Entry; 2] {
    let (ia, ib) = (a.identity(), b.identity());
    let g = net_ops::genesis(&ia, "prasiddha", "mac", "acme", 1);
    let admit = admit(&ia, &g, &ib, "bob", json!({"computer": "mbp"}));
    for env in [a, b] {
        let store = env.store();
        roster::accept(&store, &g, 10).unwrap();
        roster::accept(&store, &admit, 10).unwrap();
    }
    [g, admit]
}

/// Starts `a` and `b`, already one team (`seed_pair`), A dialing B with `a_config` and `a_env`;
/// returns once both are online.
pub fn start_pair(a: &Env, b: &Env, a_config: &str, a_env: &[(&str, String)]) {
    b.daemon(false, "", &[]);
    let mut env = vec![peers(&[b])];
    env.extend_from_slice(a_env);
    a.daemon(false, a_config, &env);
    until(10, "both online", || {
        a.status()["peers"]["online"] == 1 && b.status()["peers"]["online"] == 1
    });
}

/// Two new computers in one team (`seed_pair`), running (`start_pair`, `a_env` on A).
pub fn pair(a_env: &[(&str, String)]) -> (Env, Env) {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    start_pair(&a, &b, "", a_env);
    (a, b)
}

/// A fresh ULID, as ids and threads are.
pub fn ulid() -> String {
    ulid::Ulid::generate().to_string()
}

/// Polls `f` every 20 ms until it holds; panics after `secs`.
pub fn until(secs: u64, what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

pub fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// An executable `/bin/sh` script `dir/osascript` running `body` (a fake `osascript`).
// Written by a child `sh`, not by this process: on Linux a parallel test's fork inherits any
// write fd we hold until it execs, and exec of a file open for writing fails (ETXTBSY).
pub fn script(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    use std::io::Write;
    let p = dir.join("osascript");
    let mut sh = std::process::Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(&p)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = sh.stdin.take().unwrap();
    stdin
        .write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
        .unwrap();
    drop(stdin);
    assert!(sh.wait().unwrap().success());
    p
}

/// One request from a person: a line over `e`'s socket from a detached `perl`, which returns
/// as soon as the answer arrives (the daemon refuses person-only ops from an agent's tree).
pub fn person(e: &Env, req: Value) -> Value {
    let sock = e.state.path().join("sock");
    let script = "my $s = IO::Socket::UNIX->new(Peer => $ARGV[0]) or die $!; \
                  print $s scalar <STDIN>; print scalar <$s>;";
    let o = e.detached(
        &[
            "/usr/bin/perl",
            "-MIO::Socket::UNIX",
            "-e",
            script,
            sock.to_str().unwrap(),
        ],
        &format!("{req}\n"),
    );
    serde_json::from_slice(&o.stdout).unwrap_or_else(|_| panic!("{}{}", out(&o), err(&o)))
}

/// `LIGHTBULB_CONNECT_DIALOG`: every dialog answers `button` (empty: no answer).
pub fn dialog(button: &str) -> (&'static str, String) {
    ("LIGHTBULB_CONNECT_DIALOG", button.into())
}

/// `LIGHTBULB_CONNECT_PEERS`: the running daemons of `envs`.
pub fn peers(envs: &[&Env]) -> (&'static str, String) {
    let list: Vec<String> = envs.iter().map(|e| e.peer_arg()).collect();
    ("LIGHTBULB_CONNECT_PEERS", list.join(","))
}

/// The join wait (600 s), shortened to `s`.
pub fn wait_s(s: u64) -> (&'static str, String) {
    ("LIGHTBULB_CONNECT_DEBUG_JOIN_WAIT_S", s.to_string())
}

/// A `config.toml` line naming the computer.
pub fn computer(name: &str) -> String {
    format!("computer = \"{name}\"\n")
}

/// A fresh identity outside any state dir.
pub fn identity() -> Identity {
    Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap()
}

/// The running daemon of `env`, on the loopback.
pub fn addr(env: &Env) -> EndpointAddr {
    let key = PublicKey::from_bytes(&identity::key_bytes(&env.identity().key).unwrap()).unwrap();
    let port = env.status()["port"].as_u64().unwrap();
    EndpointAddr::new(key).with_ip_addr(format!("127.0.0.1:{port}").parse().unwrap())
}

/// A member played by the test: its own endpoint, holding `id`'s key.
pub async fn endpoint(id: &Identity) -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .secret_key(SecretKey::from_bytes(&id.signing.to_bytes()))
        .bind()
        .await
        .unwrap()
}

/// One member connection played by the test: the connection and its one stream.
pub struct Peer {
    pub conn: Connection,
    pub send: SendStream,
    pub recv: RecvStream,
}

impl Peer {
    /// Dials `env`'s daemon on the member ALPN and opens the connection's one stream.
    pub async fn dial(ep: &Endpoint, env: &Env) -> Peer {
        let conn = ep.connect(addr(env), ALPN).await.unwrap();
        let (send, recv) = conn.open_bi().await.unwrap();
        Peer { conn, send, recv }
    }

    pub async fn send(&mut self, f: &Frame) {
        frames::write(&mut self.send, f).await.unwrap();
    }

    pub async fn hello(&mut self, heads: Vec<String>) {
        let protocol = PROTOCOL;
        let addrs = vec![];
        self.send(&Frame::Hello {
            protocol,
            heads,
            addrs,
        })
        .await;
    }

    /// The next frame `pred` holds for (the others are skipped), within 5 s.
    pub async fn recv_where(&mut self, pred: impl Fn(&Frame) -> bool) -> Frame {
        loop {
            let f = tokio::time::timeout(Duration::from_secs(5), frames::read(&mut self.recv))
                .await
                .expect("a frame within 5 s")
                .unwrap();
            if pred(&f) {
                return f;
            }
        }
    }

    /// The next frame other than a `presence` (each `hello` is also answered with one).
    pub async fn recv(&mut self) -> Frame {
        self.recv_where(|f| !matches!(f, Frame::Presence { .. }))
            .await
    }

    /// The next `presence`.
    pub async fn presence(&mut self) -> Frame {
        self.recv_where(|f| matches!(f, Frame::Presence { .. }))
            .await
    }
}

/// A valid published session `session_id` (a codex one, idle).
pub fn remote(session_id: &str) -> Remote {
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

/// `member`'s presence `seq` of `sessions`, nothing known or kept.
pub fn presence_with(member: &str, seq: u64, sessions: Vec<Remote>) -> Frame {
    Frame::Presence {
        member: member.into(),
        seq,
        sessions,
        known: vec![],
        kept: vec![],
    }
}

/// Whether `env`'s `directory` lists `session_id`.
pub fn sees(env: &Env, session_id: &str) -> bool {
    let r = env.ask(json!({"id": 1, "op": "directory"}));
    r["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|s| s["session_id"] == session_id)
}

/// A member `who` played by the test, dialed in: `hello`, then `presence`.
pub async fn played(who: &Identity, env: &Env) -> (Peer, Endpoint) {
    let ep = endpoint(who).await;
    let mut p = Peer::dial(&ep, env).await;
    p.hello(vec![]).await;
    p.presence().await;
    (p, ep)
}

/// The one number `sql` selects from the database at `db`.
pub fn count(db: &std::path::Path, sql: &str) -> i64 {
    let c = rusqlite::Connection::open(db).unwrap();
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}
