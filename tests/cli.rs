//! Drives the built binary against a temp state dir and a temp HOME.

use lightbulb_network::os;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Output, Stdio};

struct Env {
    state: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        Env {
            state: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_lightbulb-network"));
        c.args(args)
            .env("LIGHTBULB_NETWORK_HOME", self.state.path())
            .env("LIGHTBULB_NETWORK_CLAUDE_HOME", self.home.path())
            .env("LIGHTBULB_NETWORK_NO_LAUNCHCTL", "1")
            .env("HOME", self.home.path())
            .env_remove("LIGHTBULB_NETWORK_SESSION")
            .env_remove("LIGHTBULB_NETWORK_HOOK_CALLER");
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    /// One daemon request on a fresh connection (the CLI starts the daemon on first use).
    fn ask(&self, body: Value) -> Value {
        let mut s = UnixStream::connect(self.state.path().join("sock")).unwrap();
        writeln!(s, "{body}").unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// Starts the daemon and registers the test process as a codex session.
    fn session(&self) -> String {
        assert!(self.run(&["who"]).status.success());
        let birth = os::process(std::process::id()).unwrap().birth_micros;
        let r = self.ask(json!({"id": 1, "op": "register", "session": {
            "provider": "codex", "conversation": "t1", "pid": std::process::id(),
            "birth_micros": birth, "cwd": "/tmp/proj", "title": "fix the thing"}}));
        r["key"].as_str().unwrap().to_string()
    }

    fn short(&self) -> String {
        self.ask(json!({"id": 1, "op": "status"}))["member_key_short"]
            .as_str()
            .unwrap()
            .to_string()
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

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn who_lists_a_registered_session() {
    let e = Env::new();
    let key = e.session();
    let table = out(&e.run(&["who"]));
    let header = table.lines().next().unwrap();
    let cols: Vec<&str> = header.split_whitespace().collect();
    assert_eq!(
        cols,
        ["session", "agent", "folder", "status", "tier", "doing"]
    );
    let row = table.lines().find(|l| l.starts_with(&key)).unwrap();
    assert!(row.contains("codex") && row.contains("/tmp/proj") && row.contains("fix the thing"));
    let json: Value = serde_json::from_str(&out(&e.run(&["who", "--json"]))).unwrap();
    assert_eq!(json[0]["key"], key.as_str());
}

#[test]
fn send_from_terminal_to_a_session_prints_stored() {
    let e = Env::new();
    let key = e.session();
    let o = e.run(&["send", &key, "hello", "there", "--request-id", "r1"]);
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(out(&o).trim(), "stored r1");
    // The sender was this terminal's name address, and the text kept its spaces.
    let r = e.ask(json!({"id": 1, "op": "receipt", "from": format!("{}/terminal", e.short()), "request_id": "r1"}));
    assert_eq!(r["receipt"]["state"], "stored");
    let m = e.ask(json!({"id": 2, "op": "inbox", "key": key}));
    assert_eq!(m["messages"][0]["text"], "hello there");
    // A group from a terminal goes to every live session, one line each.
    let o = e.run(&["send", "*", "all hands"]);
    let text = out(&o);
    assert_eq!(text.lines().count(), 1, "{text}{}", err(&o));
    assert!(text.starts_with("stored ") && text.trim_end().ends_with(&key));
}

#[test]
fn send_with_group_target_from_a_session_is_refused() {
    let e = Env::new();
    let key = e.session();
    let sent_nothing = |o: &Output| {
        assert!(!o.status.success());
        assert!(
            err(o).contains("group sends are for people at a terminal"),
            "{}",
            err(o)
        );
    };
    sent_nothing(
        &e.cmd(&["send", "*", "hi"])
            .env("LIGHTBULB_NETWORK_SESSION", &key)
            .output()
            .unwrap(),
    );
    sent_nothing(&e.run(&["send", "*", "hi", "--from", &key]));
    let short = e.short();
    sent_nothing(&e.run(&["send", &format!("{short}/*"), "hi", "--from", &key]));
    // The agent-ancestry route: the (debug-build) caller override names a process.
    let birth = os::process(std::process::id()).unwrap().birth_micros;
    sent_nothing(
        &e.cmd(&["send", "*", "hi"])
            .env(
                "LIGHTBULB_NETWORK_HOOK_CALLER",
                format!("{}:{birth}", std::process::id()),
            )
            .output()
            .unwrap(),
    );
    assert!(
        e.ask(json!({"id": 3, "op": "inbox", "key": key}))["messages"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn receipt_prints_the_state() {
    let e = Env::new();
    let key = e.session();
    assert!(e
        .run(&["send", &key, "hi", "--request-id", "rc1"])
        .status
        .success());
    let o = e.run(&["receipt", "rc1"]);
    let r: Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(
        (r["request_id"].as_str(), r["state"].as_str()),
        (Some("rc1"), Some("stored"))
    );
    let none = e.run(&["receipt", "nope"]);
    assert!(!none.status.success() && err(&none).contains("no receipt"));
}

#[test]
fn listen_receives_a_message_addressed_to_the_name() {
    let e = Env::new();
    e.session();
    let to = format!("{}/bob", e.short());
    let mut child = e
        .cmd(&["listen", "--as", "bob"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // Mail sent before or after the listener attaches both arrive (names never expire).
    assert!(e.run(&["send", &to, "ping bob"]).status.success());
    // The framed text is several lines: read until the body shows.
    let mut line = String::new();
    let mut lines = BufReader::new(child.stdout.take().unwrap());
    while !line.contains("ping bob") && lines.read_line(&mut line).unwrap() > 0 {}
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(line.contains("ping bob"), "{line}");
}

#[test]
fn send_to_a_bare_name_reaches_listen() {
    let e = Env::new();
    e.session();
    let o = e.run(&["send", "carol", "bare hello", "--request-id", "b1"]);
    assert_eq!(out(&o).trim(), "stored b1", "{}", err(&o));
    let o = e.run(&["listen", "--as", "carol", "--once"]);
    assert!(out(&o).contains("bare hello"), "{}", err(&o));
}

#[test]
fn listen_once_prints_a_framed_message() {
    let e = Env::new();
    e.session();
    let to = format!("{}/ann", e.short());
    assert!(e
        .run(&["send", &to, "first", "--request-id", "q1"])
        .status
        .success());
    let o = e.run(&["listen", "--as", "ann", "--once"]);
    assert!(o.status.success(), "{}", err(&o));
    let text = out(&o);
    // The framed text carries the request id and the body, not the raw JSON.
    assert!(text.contains("first") && text.contains("q1"), "{text}");
    assert!(!text.contains("\"text\""));
    assert!(!e
        .run(&["listen", "--as", "no spaces", "--once"])
        .status
        .success());
}

#[test]
fn an_unknown_subcommand_forwards_or_explains() {
    let e = Env::new();
    let missing = e
        .cmd(&["claude", "--resume"])
        .env("LIGHTBULB_NETWORK_DESKTOP", "/nonexistent/app")
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(2));
    assert!(err(&missing).contains("needs the Lightbulb app: thelightbulb.company/download"));

    let app = e.home.path().join("fake-app");
    std::fs::write(&app, "#!/bin/sh\necho \"forwarded: $@\"\n").unwrap();
    std::fs::set_permissions(&app, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    for sub in ["claude", "share", "frobnicate"] {
        let o = e
            .cmd(&[sub, "a", "--b"])
            .env("LIGHTBULB_NETWORK_DESKTOP", &app)
            .output()
            .unwrap();
        assert_eq!(
            out(&o).trim(),
            format!("forwarded: {sub} a --b"),
            "{}",
            err(&o)
        );
    }
}

#[test]
fn daemon_install_writes_the_plist_and_alias_without_touching_a_foreign_file() {
    let e = Env::new();
    let bin = e.home.path().join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("lightbulb"), "mine").unwrap();

    let o = e.run(&["daemon", "--install"]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(out(&o).contains("not ours; left alone"), "{}", out(&o));
    // Under NO_LAUNCHCTL the line records the sequence a real run performs.
    assert!(out(&o).contains("would run: bootout gui/"), "{}", out(&o));
    assert!(out(&o).contains("; bootstrap gui/") && out(&o).contains("; kickstart -k gui/"));
    assert_eq!(
        std::fs::read_to_string(bin.join("lightbulb")).unwrap(),
        "mine"
    );

    let plist = std::fs::read_to_string(
        e.home
            .path()
            .join("Library/LaunchAgents/company.thelightbulb.network.plist"),
    )
    .unwrap();
    let canon = e
        .home
        .path()
        .join(".lightbulb/network/bin/lightbulb-network");
    assert!(plist.contains("<string>company.thelightbulb.network</string>"));
    assert!(plist.contains(&format!(
        "<string>{}</string><string>daemon</string>",
        canon.display()
    )));
    assert!(
        plist.contains("<key>RunAtLoad</key><true/>")
            && plist.contains("<key>KeepAlive</key><true/>")
    );
    assert!(plist.contains(&format!("{}/daemon.log", e.state.path().display())));
    assert!(canon.is_file());

    std::fs::remove_file(bin.join("lightbulb")).unwrap();
    assert!(e.run(&["daemon", "--install"]).status.success());
    assert_eq!(std::fs::read_link(bin.join("lightbulb")).unwrap(), canon);

    // A symlink pointing elsewhere is foreign too.
    std::fs::remove_file(bin.join("lightbulb")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/true", bin.join("lightbulb")).unwrap();
    assert!(out(&e.run(&["daemon", "--install"])).contains("not ours"));
    assert_eq!(
        std::fs::read_link(bin.join("lightbulb")).unwrap().to_str(),
        Some("/usr/bin/true")
    );

    let o = e.run(&["daemon", "--uninstall"]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(!e
        .home
        .path()
        .join("Library/LaunchAgents/company.thelightbulb.network.plist")
        .exists());
    assert!(e.state.path().exists());
}

#[test]
fn leave_and_doctor() {
    let e = Env::new();
    let o = e.run(&["leave"]);
    assert!(o.status.success());
    assert_eq!(out(&o).trim(), "This computer is on no network yet.");
    // Nothing is installed in the temp home: doctor reports it and exits 1.
    let d = e.run(&["doctor"]);
    assert_eq!(d.status.code(), Some(1));
    assert!(out(&d).contains("alias: missing") && out(&d).contains("daemon: missing"));
}
