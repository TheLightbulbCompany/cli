//! Drives the built binary against a temp state dir and a temp HOME.

use lightbulb_connect::os;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::process::{Command, Output, Stdio};

mod common;

use common::{err, out, Env, EXE};

impl Env {
    /// `lightbulb-connect args` as a person at a terminal: detached (so no agent the tests run
    /// under is above it, which the daemon checks for `<member>/terminal` sends) under a
    /// program that is neither a shell nor an agent (`perl`), where the CLI's own walk stops.
    fn person(&self, args: &[&str]) -> Output {
        let run = "exit(system(@ARGV) >> 8)";
        let argv: Vec<&str> = ["/usr/bin/perl", "-e", run, EXE]
            .into_iter()
            .chain(args.iter().copied())
            .collect();
        self.detached(&argv, "")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_lightbulb-connect"));
        c.args(args)
            .env("LIGHTBULB_CONNECT_HOME", self.state.path())
            .env("LIGHTBULB_CONNECT_CLAUDE_HOME", self.home.path())
            .env("LIGHTBULB_CONNECT_NO_LAUNCHCTL", "1")
            .env("HOME", self.home.path())
            .env_remove("LIGHTBULB_CONNECT_SESSION")
            .env_remove("LIGHTBULB_CONNECT_HOOK_CALLER");
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
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
    let o = e.person(&["send", &key, "hello", "there", "--request-id", "r1"]);
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(out(&o).trim(), "stored r1");
    // The sender was this terminal's name address, and the text kept its spaces.
    let r = e.ask(json!({"id": 1, "op": "receipt", "from": format!("{}/terminal", e.short()), "request_id": "r1"}));
    assert_eq!(r["receipt"]["state"], "stored");
    let m = e.ask(json!({"id": 2, "op": "inbox", "key": key}));
    assert_eq!(m["messages"][0]["text"], "hello there");
    // A group from a terminal goes to every live session, one line each.
    let o = e.person(&["send", "*", "all hands"]);
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
            .env("LIGHTBULB_CONNECT_SESSION", &key)
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
                "LIGHTBULB_CONNECT_HOOK_CALLER",
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
        .person(&["send", &key, "hi", "--request-id", "rc1"])
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
    assert!(e.person(&["send", &to, "ping bob"]).status.success());
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
    let o = e.person(&["send", "carol", "bare hello", "--request-id", "b1"]);
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
        .person(&["send", &to, "first", "--request-id", "q1"])
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
        .env("LIGHTBULB_CONNECT_DESKTOP", "/nonexistent/app")
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
            .env("LIGHTBULB_CONNECT_DESKTOP", &app)
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
            .join("Library/LaunchAgents/company.thelightbulb.connect.plist"),
    )
    .unwrap();
    let canon = e
        .home
        .path()
        .join(".lightbulb/connect/bin/lightbulb-connect");
    assert!(plist.contains("<string>company.thelightbulb.connect</string>"));
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
        .join("Library/LaunchAgents/company.thelightbulb.connect.plist")
        .exists());
    assert!(e.state.path().exists());
}

#[test]
fn doctor_reports_what_is_missing() {
    // Nothing is installed in the temp home: doctor reports it and exits 1.
    let e = Env::new();
    let d = e.run(&["doctor"]);
    assert_eq!(d.status.code(), Some(1));
    assert!(out(&d).contains("alias: missing") && out(&d).contains("daemon: missing"));
}

#[test]
fn doctor_reports_a_foreign_alias_as_not_ours_not_missing() {
    let e = Env::new();
    let bin = e.home.path().join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("/usr/bin/true", bin.join("lightbulb")).unwrap();
    let d = out(&e.run(&["doctor"]));
    assert!(d.contains("alias: not ours (left alone)"), "{d}");
    assert!(!d.contains("alias: missing"), "{d}");
}

#[test]
fn the_frame_names_the_terminal_and_name_senders() {
    let e = Env::new();
    let key = e.session();
    assert!(e.person(&["send", &key, "from me"]).status.success());
    let m = e.ask(json!({"id": 1, "op": "inbox", "key": key}));
    let framed = m["messages"][0]["framed"].as_str().unwrap().to_string();
    assert!(
        framed.starts_with("[Lightbulb teammate contribution from You, at the terminal {"),
        "{framed}"
    );
    // A name address is named as the person's mailbox (`label::render`).
    let short = e.short();
    let r = e.ask(
        json!({"id": 2, "op": "send", "from": format!("{short}/desk"), "to": key,
        "text": "x", "request_id": "n1"}),
    );
    assert_eq!(r["receipt"]["state"], "stored", "{r}");
    let m = e.ask(json!({"id": 3, "op": "inbox", "key": key}));
    let framed = m["messages"][0]["framed"].as_str().unwrap();
    assert!(
        framed.starts_with("[Lightbulb teammate contribution from Your desk mailbox {"),
        "{framed}"
    );
}

#[test]
fn send_from_an_agent_shell_sends_as_its_session_or_refuses() {
    let e = Env::new();
    let key = e.session(); // this test process, as a codex session
    let birth = os::process(std::process::id()).unwrap().birth_micros;
    let o = e
        .cmd(&["send", "bob", "from my shell", "--request-id", "s1"])
        .env(
            "LIGHTBULB_CONNECT_HOOK_CALLER",
            format!("{}:{birth}", std::process::id()),
        )
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", err(&o));
    let m = e.ask(json!({"id": 1, "op": "inbox", "key": format!("{}/bob", e.short())}));
    assert_eq!(m["messages"][0]["from"], key.as_str(), "{m}");
    assert!(m["messages"][0]["framed"]
        .as_str()
        .unwrap()
        .contains("from Your Codex {"));
    // --from cannot speak for the terminal (or anyone else) from that shell; its own is fine.
    let pinned = |from: &str| {
        e.cmd(&["send", "bob", "x", "--from", from])
            .env(
                "LIGHTBULB_CONNECT_HOOK_CALLER",
                format!("{}:{birth}", std::process::id()),
            )
            .output()
            .unwrap()
    };
    let o = pinned(&format!("{}/terminal", e.short()));
    assert!(!o.status.success());
    assert!(err(&o).contains("your own session"), "{}", err(&o));
    let o = pinned(&key);
    assert!(o.status.success(), "{}", err(&o));

    // An agent above that no session belongs to: refused, nothing sent.
    let other = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let ob = os::process(other.id()).unwrap().birth_micros;
    let o = e
        .cmd(&["send", "bob", "who am i"])
        .env(
            "LIGHTBULB_CONNECT_HOOK_CALLER",
            format!("{}:{ob}", other.id()),
        )
        .output()
        .unwrap();
    let mut other = other;
    let _ = other.kill();
    let _ = other.wait();
    assert!(!o.status.success());
    assert!(
        err(&o)
            .contains("send from an agent's shell must name --from <session_id>, your own session"),
        "{}",
        err(&o)
    );
}

#[test]
fn setup_installs_the_canonical_binary_and_wires_everything_to_it() {
    let e = Env::new();
    let h = e.home.path();
    std::fs::create_dir(h.join(".claude")).unwrap();
    let canon = h.join(".lightbulb/connect/bin/lightbulb-connect");

    // A missing --executable is refused before anything is written.
    let o = e.run(&["setup", "--executable", "/nonexistent/lightbulb-connect"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("does not exist"), "{}", err(&o));
    assert!(!h.join(".claude.json").exists());

    let o = e.run(&["setup"]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(
        canon.is_file(),
        "the running binary is copied to the canonical path"
    );
    let canon_s = canon.to_str().unwrap();
    let mcp: Value =
        serde_json::from_str(&std::fs::read_to_string(h.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(mcp["mcpServers"]["lightbulb"]["command"], canon_s);
    let hooks = std::fs::read_to_string(h.join(".claude/settings.json")).unwrap();
    assert!(
        hooks.contains(&format!("'{canon_s}' hook claude ")),
        "{hooks}"
    );
    assert!(!hooks.contains("target/"), "{hooks}");
    if cfg!(target_os = "macos") {
        let plist = std::fs::read_to_string(
            h.join("Library/LaunchAgents/company.thelightbulb.connect.plist"),
        )
        .unwrap();
        assert!(plist.contains(&format!(
            "<string>{canon_s}</string><string>daemon</string>"
        )));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn setup_remove_also_stops_the_daemon_and_removes_the_job() {
    let e = Env::new();
    let h = e.home.path();
    assert!(e.run(&["setup"]).status.success());
    let plist = h.join("Library/LaunchAgents/company.thelightbulb.connect.plist");
    assert!(plist.exists());
    let o = e.run(&["setup", "--remove"]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(!plist.exists());
    assert!(
        out(&o).contains("daemon stopped and launchd job removed"),
        "{}",
        out(&o)
    );
}

#[test]
fn folder_off_hides_sessions_after_reload() {
    let (a, b) = common::pair(&[]);
    let proj = tempfile::tempdir().unwrap();
    let dir = proj.path().to_str().unwrap().to_string();
    let p = os::process(std::process::id()).unwrap();
    let register = |conv: &str, cwd: &str| {
        a.ask(
            json!({"id": 1, "op": "register", "session": {"provider": "codex",
            "conversation": conv, "pid": p.pid, "birth_micros": p.birth_micros, "cwd": cwd}}),
        )
    };
    let s1 = register("f1", &dir)["key"].as_str().unwrap().to_string();
    common::until(15, "B sees the session", || common::sees(&b, &s1));
    // A send from B's terminal to it says where it waits.
    let o = b.person(&["send", &s1, "hello"]);
    let line = out(&o);
    assert!(
        line.contains(" stored on mac (tier ")
            && line.contains("). Receipt: lightbulb-connect receipt "),
        "{line}{}",
        err(&o)
    );

    let o = a.person(&["folder", &dir, "off"]);
    assert!(o.status.success(), "{}{}", out(&o), err(&o));
    let config = std::fs::read_to_string(a.state.path().join("config.toml")).unwrap();
    assert!(config.contains("folders_off"), "{config}");
    common::until(15, "B no longer sees it", || !common::sees(&b, &s1));
    // A new session under the folder is not registered at all.
    let sub = proj.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let r = register("f2", sub.to_str().unwrap());
    assert_eq!(r["ignored"], true, "{r}");
    let local = a.ask(json!({"id": 1, "op": "sessions"}));
    assert!(!local.to_string().contains("codex:f2"), "{local}");

    let o = a.person(&["folder", &dir, "on"]);
    assert!(o.status.success(), "{}{}", out(&o), err(&o));
    common::until(15, "B sees it again", || common::sees(&b, &s1));
}
