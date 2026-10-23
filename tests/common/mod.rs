//! What the CLI and network test files share: a temp state dir and HOME, a daemon shut down
//! on drop, a person's shell (`detached`) and socket client (`person`), the daemon env knobs,
//! a two-member team seeded before start, and a member played by the test (`Peer`).
//! ponytail: only `cli.rs`, `team_cli.rs`, `net_two.rs`, `join.rs`, `ui_claim.rs` and `dialog.rs` use it;
//! the other test files keep their own copies.
#![allow(dead_code)]

use iroh::endpoint::{presets, Connection, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, PublicKey, SecretKey};
use lightbulb_network::daemon::net_ops;
use lightbulb_network::identity::{self, Identity};
use lightbulb_network::net::frames::{self, Frame, Remote};
use lightbulb_network::net::{ALPN, PROTOCOL};
use lightbulb_network::roster::{self, Entry, Kind};
use lightbulb_network::store::Store;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

pub const EXE: &str = env!("CARGO_BIN_EXE_lightbulb-network");

pub struct Env {
    pub state: tempfile::TempDir,
    pub home: tempfile::TempDir,
    /// `LIGHTBULB_NETWORK_HOOK_CALLER` for the daemon: the process it treats as an agent.
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
    fn base_cmd(&self, mut c: Command) -> Command {
        c.env("LIGHTBULB_NETWORK_HOME", self.state.path())
            .env("LIGHTBULB_NETWORK_CLAUDE_HOME", self.home.path())
            .env("LIGHTBULB_NETWORK_NO_LAUNCHCTL", "1")
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

    /// Starts `lightbulb-network daemon` on this state dir with `lan` (mDNS) and `config` as
    /// `config.toml`, and `env` set, and waits until it answers.
    pub fn daemon(&self, lan: bool, config: &str, env: &[(&str, String)]) {
        let config = format!("lan = {lan}\n{config}");
        std::fs::write(self.state.path().join("config.toml"), config).unwrap();
        let log = std::fs::File::create(self.state.path().join("daemon.log")).unwrap();
        let mut c = self.base_cmd(Command::new(EXE));
        c.arg("daemon")
            .env_remove("LIGHTBULB_NETWORK_PEERS")
            .env_remove("LIGHTBULB_NETWORK_DEBUG_PROTOCOL")
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
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

    pub fn status(&self) -> Value {
        self.ask(json!({"id": 1, "op": "status"}))
    }

    pub fn heads(&self) -> Value {
        self.ask(json!({"id": 1, "op": "team.get"}))["heads"].clone()
    }

    /// `<member key>@127.0.0.1:<port>`: this running daemon, for `LIGHTBULB_NETWORK_PEERS`.
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
            .env_remove("LIGHTBULB_NETWORK_HOOK_CALLER");
        if let Some(pin) = &self.pin {
            c.env("LIGHTBULB_NETWORK_HOOK_CALLER", pin);
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

/// The same team in both stores before either daemon starts: `a` founds "acme" (computer
/// "mac") and admits `b` as "bob" (computer "mbp"). Returns `[genesis, admit]`.
pub fn seed_pair(a: &Env, b: &Env) -> [Entry; 2] {
    let (ia, ib) = (a.identity(), b.identity());
    let g = net_ops::genesis(&ia, "prasiddha", "mac", "acme", 1);
    let admit = Entry::signed(
        &ia,
        vec![g.hash()],
        Kind::Admit,
        json!({"member": ib.key, "name": "bob", "computer": "mbp", "person_id": ib.key,
               "kind": "computer", "role": "member"}),
        2,
    );
    for env in [a, b] {
        let store = env.store();
        roster::accept(&store, &g, 10).unwrap();
        roster::accept(&store, &admit, 10).unwrap();
    }
    [g, admit]
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
pub fn script(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("osascript");
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
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

/// `LIGHTBULB_NETWORK_DIALOG`: every dialog answers `button` (empty: no answer).
pub fn dialog(button: &str) -> (&'static str, String) {
    ("LIGHTBULB_NETWORK_DIALOG", button.into())
}

/// `LIGHTBULB_NETWORK_PEERS`: the running daemons of `envs`.
pub fn peers(envs: &[&Env]) -> (&'static str, String) {
    let list: Vec<String> = envs.iter().map(|e| e.peer_arg()).collect();
    ("LIGHTBULB_NETWORK_PEERS", list.join(","))
}

/// The join wait (600 s), shortened to `s`.
pub fn wait_s(s: u64) -> (&'static str, String) {
    ("LIGHTBULB_NETWORK_DEBUG_JOIN_WAIT_S", s.to_string())
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
        self.send(&Frame::Hello { protocol, heads }).await;
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
