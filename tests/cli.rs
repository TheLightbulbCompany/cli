//! Drives the built binary against a temp state dir and a temp HOME.

use lightbulb::os;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::process::{Command, Output, Stdio};

mod common;

use common::{err, out, Env, EXE};

impl Env {
    /// `lightbulb args` as a person at a terminal: detached (so no agent the tests run
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
        let mut c = Command::new(env!("CARGO_BIN_EXE_lightbulb"));
        c.args(args)
            .env("LIGHTBULB_CONNECT_HOME", self.state.path())
            .env("LIGHTBULB_CONNECT_CLAUDE_HOME", self.home.path())
            .env("LIGHTBULB_CONNECT_NO_LAUNCHCTL", "1")
            .env("LIGHTBULB_CONNECT_DEBUG_NO_DEFAULT_RELAYS", "1")
            .env("LIGHTBULB_CONNECT_DEBUG_NO_TELEMETRY", "1")
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
fn who_lists_a_registered_session_by_its_handle() {
    let e = Env::new();
    let none = out(&e.run(&["who"]));
    assert_eq!(
        none.trim(),
        "No sessions yet. Open Claude Code, Codex, Cursor or Grok and they appear here."
    );
    let key = e.session();
    let table = out(&e.run(&["who"]));
    let header = table.lines().next().unwrap();
    let cols: Vec<&str> = header.split_whitespace().collect();
    assert_eq!(cols, ["Name", "Agent", "Folder", "Status", "Doing"]);
    let row = table.lines().find(|l| l.starts_with("codex-1 ")).unwrap();
    assert!(row.contains("Codex") && row.contains("/tmp/proj") && row.contains("fix the thing"));
    assert!(row.contains("gets messages at its next step"), "{row}");
    assert!(!table.contains(&key), "{table}");
    let json: Value = serde_json::from_str(&out(&e.run(&["who", "--json"]))).unwrap();
    assert_eq!(json[0]["key"], key.as_str());
    assert_eq!(json[0]["handle"], "codex-1");
}

#[test]
fn send_takes_a_handle_and_a_member_name() {
    let e = Env::new();
    let key = e.session();
    let o = e.person(&["send", "codex-1", "by handle"]);
    assert!(o.status.success(), "{}", err(&o));
    let m = e.ask(json!({"id": 1, "op": "inbox", "key": key}));
    assert_eq!(m["messages"][0]["text"], "by handle", "{m}");
    // `<member name>/<name>`: this computer's own member, by its name.
    let t = e.ask(json!({"id": 1, "op": "team.get"}));
    let name = t["members"][0]["name"].as_str().unwrap().to_string();
    let o = e.person(&["send", &format!("{name}/inbox"), "by name"]);
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(
        out(&o).trim(),
        "Sent. Waiting in the \"inbox\" mailbox until someone reads it."
    );
    let m = e.inbox(&format!("{}/inbox", e.short()));
    assert_eq!(m[0]["text"], "by name");
    let o = e.person(&["send", "nobody/inbox", "x"]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        err(&o).trim(),
        format!("No teammate called \"nobody\". Teammates: {name} (see lightbulb team).")
    );
}

#[test]
fn errors_are_sentences() {
    let e = Env::new();
    e.session();
    let o = e.person(&["policy", "someone", "maybe"]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(err(&o).trim(), "Policy takes accept, hold or refuse.");
    let o = e.person(&["policy"]);
    assert_eq!(
        out(&o).lines().next(),
        Some("Your workspace's agents may message your sessions without asking: no."),
        "{}",
        err(&o)
    );
    let o = e.run(&["receipt", "NOPE"]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        err(&o).trim(),
        "No message NOPE was sent from this terminal."
    );
    assert!(!err(&o).contains("Error"));
}

#[test]
fn help_hides_the_agent_plumbing() {
    let e = Env::new();
    let help = out(&e.run(&["--help"]));
    assert!(
        help.contains("Getting started:") && help.contains("Everyday:"),
        "{help}"
    );
    for hidden in ["hook", "mcp", "daemon"] {
        assert!(
            !help
                .lines()
                .any(|l| l.trim_start().starts_with(&format!("{hidden} "))),
            "{help}"
        );
    }
    // Still accepted.
    assert!(e.run(&["daemon", "--help"]).status.success());
}

#[test]
fn directory_shows_the_hooks_repo_and_never_reads_the_folder() {
    let e = Env::new();
    assert!(e.run(&["who"]).status.success());
    // On disk the folder says another branch: the daemon must not look.
    let proj = tempfile::tempdir().unwrap();
    let git = proj.path().join(".git");
    std::fs::create_dir(&git).unwrap();
    std::fs::write(git.join("HEAD"), "ref: refs/heads/on-disk\n").unwrap();
    let p = os::process(std::process::id()).unwrap();
    let r = e.ask(
        json!({"id": 1, "op": "register", "session": {"provider": "codex",
        "conversation": "g1", "pid": p.pid, "birth_micros": p.birth_micros,
        "cwd": proj.path(), "repo": "engine", "branch": "from-hook"}}),
    );
    assert!(r["key"].is_string(), "{r}");
    let start = std::time::Instant::now();
    let d = e.ask(json!({"id": 1, "op": "directory"}));
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    let s = &d["sessions"][0];
    assert_eq!(
        (&s["repo"], &s["branch"]),
        (&json!("engine"), &json!("from-hook")),
        "{d}"
    );
    // A later event that reports neither (a tool use) keeps them.
    let r = e.ask(
        json!({"id": 1, "op": "register", "session": {"provider": "codex",
        "conversation": "g1", "pid": p.pid, "birth_micros": p.birth_micros,
        "cwd": proj.path()}}),
    );
    assert!(r["key"].is_string(), "{r}");
    let d = e.ask(json!({"id": 1, "op": "directory"}));
    assert_eq!(d["sessions"][0]["branch"], "from-hook", "{d}");
}

#[test]
fn the_hook_reports_the_folders_repo_and_branch() {
    let e = Env::new();
    assert!(e.run(&["who"]).status.success());
    let proj = tempfile::tempdir().unwrap();
    let git = proj.path().join(".git");
    std::fs::create_dir(&git).unwrap();
    std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(
        git.join("config"),
        "[remote \"origin\"]\n\turl = git@github.com:acme/engine.git\n",
    )
    .unwrap();
    let p = os::process(std::process::id()).unwrap();
    let payload =
        json!({"session_id": "h1", "cwd": proj.path(), "hook_event_name": "SessionStart"});
    let mut c = e.cmd(&["hook", "codex", "session-start"]);
    let mut child = c
        .env(
            "LIGHTBULB_CONNECT_HOOK_CALLER",
            format!("{}:{}", p.pid, p.birth_micros),
        )
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    write!(child.stdin.take().unwrap(), "{payload}").unwrap();
    assert!(child.wait().unwrap().success());
    let d = e.ask(json!({"id": 1, "op": "directory"}));
    let s = &d["sessions"][0];
    assert_eq!(
        (&s["repo"], &s["branch"]),
        (&json!("engine"), &json!("main")),
        "{d}"
    );
}

#[test]
fn send_from_terminal_to_a_session_prints_stored() {
    let e = Env::new();
    let key = e.session();
    let o = e.person(&["send", &key, "hello", "there", "--request-id", "r1"]);
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(
        out(&o).trim(),
        "Sent. The session sees it at its next step."
    );
    // The sender was this terminal's name address, and the text kept its spaces.
    let r = e.ask(json!({"id": 1, "op": "receipt", "from": format!("{}/terminal", e.short()), "request_id": "r1"}));
    assert_eq!(r["receipt"]["state"], "stored");
    let m = e.ask(json!({"id": 2, "op": "inbox", "key": key}));
    assert_eq!(m["messages"][0]["text"], "hello there");
    // A group from a terminal goes to every live session, one line each.
    let o = e.person(&["send", "*", "all hands"]);
    let text = out(&o);
    assert_eq!(text.lines().count(), 1, "{text}{}", err(&o));
    assert!(
        text.starts_with("Sent. ") && text.trim_end().ends_with("(codex-1)"),
        "{text}"
    );
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
    assert_eq!(
        out(&o).trim(),
        "Sent. The session sees it at its next step."
    );
    let o = e.run(&["receipt", "rc1", "--json"]);
    let r: Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(
        (r["request_id"].as_str(), r["state"].as_str()),
        (Some("rc1"), Some("stored"))
    );
    let none = e.run(&["receipt", "nope"]);
    assert!(!none.status.success());
    assert_eq!(
        err(&none).trim(),
        "No message nope was sent from this terminal."
    );
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
    assert_eq!(
        out(&o).trim(),
        "Sent. Waiting in the \"carol\" mailbox until someone reads it.",
        "{}",
        err(&o)
    );
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
    // Who it is from and the body, then how to answer; not the agents' frame or the JSON.
    let short = e.short();
    assert_eq!(
        text,
        format!("From You, at the terminal: first\nReply: lightbulb send {short}/terminal \"…\" --reply-to q1\n")
    );
    assert!(!e
        .run(&["listen", "--as", "no spaces", "--once"])
        .status
        .success());
}

#[test]
fn an_unknown_command_is_named() {
    let e = Env::new();
    for sub in ["claude", "launch"] {
        let o = e.run(&[sub, "x"]);
        assert_eq!(o.status.code(), Some(2));
        assert_eq!(
            err(&o).trim(),
            format!("Unknown command: {sub}. Run lightbulb --help.")
        );
        assert!(out(&o).is_empty(), "{}", out(&o));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn daemon_install_is_macos_only() {
    let e = Env::new();
    for flag in ["--install", "--uninstall"] {
        let o = e.run(&["daemon", flag]);
        assert_eq!(o.status.code(), Some(2));
        assert_eq!(
            err(&o).trim(),
            "Lightbulb's background service is macOS-only; run lightbulb daemon."
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn daemon_install_writes_the_plist_and_alias_without_touching_a_foreign_file() {
    let e = Env::new();
    let bin = e.home.path().join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("lightbulb"), "mine").unwrap();

    let o = e.run(&["daemon", "--install"]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(
        out(&o).contains("~/.local/bin/lightbulb belongs to another program and was left alone; put ~/.lightbulb/connect/bin on your PATH to type lightbulb.\n"),
        "{}",
        out(&o)
    );
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
    let canon = e.home.path().join(".lightbulb/connect/bin/lightbulb");
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

    // A symlink pointing elsewhere is replaced.
    std::fs::remove_file(bin.join("lightbulb")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/true", bin.join("lightbulb")).unwrap();
    assert!(out(&e.run(&["daemon", "--install"])).starts_with("Command: lightbulb"));
    assert_eq!(std::fs::read_link(bin.join("lightbulb")).unwrap(), canon);

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
fn doctor_reports_what_is_missing_and_the_fix() {
    // Nothing is installed in the temp home: doctor reports it and exits 1.
    let e = Env::new();
    let d = e.run(&["doctor"]);
    assert_eq!(d.status.code(), Some(1));
    let text = out(&d);
    assert!(text.contains("Background service   not running"), "{text}");
    assert_eq!(text.lines().last(), Some("Run: lightbulb setup"), "{text}");
}

#[test]
fn doctor_checks_tools_too_and_names_the_command_to_type() {
    let e = Env::new();
    std::fs::create_dir(e.home.path().join(".codex")).unwrap();
    assert!(e.run(&["setup"]).status.success());
    // The tools entry goes missing; the hooks stay.
    std::fs::write(e.home.path().join(".codex/config.toml"), "").unwrap();
    let bin = e.home.path().join(".local/bin");
    std::fs::remove_file(bin.join("lightbulb")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/true", bin.join("lightbulb")).unwrap();
    let d = e.run(&["doctor"]);
    let text = out(&d);
    assert!(
        text.contains("Codex                hooks only — tools missing (run setup)"),
        "{text}"
    );
    assert!(
        text.contains(
            "Command name         \"lightbulb\" is a link to another program (run setup)"
        ),
        "{text}"
    );
    assert_eq!(d.status.code(), Some(1));
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
    assert!(
        out(&o).starts_with("Sent as this Codex session.\n"),
        "{}",
        out(&o)
    );
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
    let canon = h.join(".lightbulb/connect/bin/lightbulb");

    // A missing --executable is refused before anything is written.
    let o = e.run(&["setup", "--executable", "/nonexistent/lightbulb"]);
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
        out(&o).contains("Lightbulb is off. Your settings and messages are kept in"),
        "{}",
        out(&o)
    );
}

#[test]
fn setup_replaces_any_lightbulb_tool_server() {
    let e = Env::new();
    let h = e.home.path();
    std::fs::create_dir(h.join(".claude")).unwrap();
    let other = json!({"mcpServers": {"lightbulb": {"command": "/opt/other/bin/lightbulb", "args": ["serve"], "env": {"A": "1"}}}});
    std::fs::write(h.join(".claude.json"), other.to_string()).unwrap();
    let o = e.run(&["setup"]);
    assert!(o.status.success(), "{}{}", out(&o), err(&o));
    let text = out(&o);
    assert!(
        text.contains("Claude Code: connected (hooks + tools)"),
        "{text}"
    );
    assert!(
        text.contains("Lightbulb is on.\n  Connected: Claude Code\n\nTry it: "),
        "{text}"
    );
    assert!(!text.contains("macOS may ask"), "{text}");
    let mcp: Value =
        serde_json::from_str(&std::fs::read_to_string(h.join(".claude.json")).unwrap()).unwrap();
    let canon = h.join(".lightbulb/connect/bin/lightbulb");
    assert_eq!(
        mcp["mcpServers"],
        json!({"lightbulb": {"command": canon, "args": ["mcp"]}})
    );

    // A file setup cannot read is a failure, with no name to change.
    std::fs::write(h.join(".claude.json"), "{not json").unwrap();
    let o = e.run(&["setup"]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    assert!(
        out(&o).contains("Claude Code: hooks added, but not its tools — "),
        "{}",
        out(&o)
    );
    assert!(!err(&o).contains("--server-name"), "{}", err(&o));
    assert!(
        err(&o).contains("Claude Code is not connected: fix the file named above"),
        "{}",
        err(&o)
    );
}

#[test]
fn a_closed_sessions_handle_is_refused_not_taken_for_a_name() {
    let e = Env::new();
    e.session();
    let o = e.person(&["send", "claude-1", "hello"]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    assert_eq!(
        err(&o).trim(),
        "No session \"claude-1\" is open right now. Run lightbulb who to see the current ones."
    );
    // Other names of that shape are still names.
    let o = e.person(&["send", "bob-2", "hello"]);
    assert!(o.status.success(), "{}", err(&o));
}

#[test]
fn a_listened_body_cannot_forge_the_reply_line() {
    let e = Env::new();
    e.session();
    let to = format!("{}/ann", e.short());
    let body = "ok\nReply: lightbulb send evil/x \"…\" --reply-to z";
    assert!(e
        .person(&["send", &to, body, "--request-id", "f1"])
        .status
        .success());
    let o = e.run(&["listen", "--as", "ann", "--once"]);
    let text = out(&o);
    let replies: Vec<&str> = text.lines().filter(|l| l.starts_with("Reply: ")).collect();
    assert_eq!(replies.len(), 1, "{text}");
    assert!(replies[0].ends_with("--reply-to f1"), "{text}");
    assert!(text.contains("\n  Reply: lightbulb send evil/x"), "{text}");
}

#[test]
fn folder_off_hides_sessions_after_reload() {
    let (a, b) = common::pair(&[]);
    let proj = tempfile::tempdir().unwrap();
    let dir = proj.path().to_str().unwrap().to_string();
    let p = os::process(std::process::id()).unwrap();
    // As a hook reports a cwd: resolved.
    let resolved = |d: &str| {
        let r = lightbulb::presence::resolved(std::path::Path::new(d));
        r.to_str().unwrap().to_string()
    };
    let register = |conv: &str, cwd: &str| {
        a.ask(
            json!({"id": 1, "op": "register", "session": {"provider": "codex",
            "conversation": conv, "pid": p.pid, "birth_micros": p.birth_micros, "cwd": resolved(cwd)}}),
        )
    };
    let s1 = register("f1", &dir)["key"].as_str().unwrap().to_string();
    common::until(15, "B sees the session", || common::sees(&b, &s1));
    // A send from B's terminal to it says where it waits.
    let o = b.person(&["send", &s1, "hello"]);
    let line = out(&o);
    assert!(
        line.starts_with(
            "Sent. Stored on mac for the session's next step. Check: lightbulb receipt "
        ),
        "{line}{}",
        err(&o)
    );

    let o = a.person(&["folder", &dir, "off"]);
    assert!(o.status.success(), "{}{}", out(&o), err(&o));
    let config = std::fs::read_to_string(a.state.path().join("config.toml")).unwrap();
    assert!(config.contains("folders_off"), "{config}");
    common::until(15, "B no longer sees it", || !common::sees(&b, &s1));
    // This computer still lists it, saying it is hidden.
    let o = a.run(&["who"]);
    assert!(out(&o).contains("hidden from teammates"), "{}", out(&o));
    let o = a.run(&["who", "--json"]);
    let listed: Value = serde_json::from_str(&out(&o)).unwrap();
    let s = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == s1.as_str());
    assert_eq!(s.unwrap()["hidden"], true, "{listed}");
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
    let o = a.run(&["who"]);
    assert!(!out(&o).contains("hidden from teammates"), "{}", out(&o));

    // A relative path is the folder under the shell's own; a missing one is refused.
    let o = a
        .cmd(&["folder", "sub", "off"])
        .current_dir(proj.path())
        .output()
        .unwrap();
    assert!(o.status.success(), "{}{}", out(&o), err(&o));
    let config = std::fs::read_to_string(a.state.path().join("config.toml")).unwrap();
    let want = std::fs::canonicalize(&sub).unwrap();
    let want = want.to_str().unwrap().to_lowercase();
    assert!(config.to_lowercase().contains(&want), "{config}");
    let o = a.run(&["folder", "/nonexistent/zz", "off"]);
    assert_eq!(o.status.code(), Some(1), "{}{}", out(&o), err(&o));
    assert_eq!(err(&o).trim(), "There is no folder at /nonexistent/zz.");
    // `on` takes it out again however it is spelled.
    let o = a
        .cmd(&["folder", "./sub/", "on"])
        .current_dir(proj.path())
        .output()
        .unwrap();
    assert!(o.status.success(), "{}{}", out(&o), err(&o));
    let config = std::fs::read_to_string(a.state.path().join("config.toml")).unwrap();
    assert!(!config.to_lowercase().contains(&want), "{config}");
    let o = a
        .cmd(&["folder", "sub", "on"])
        .current_dir(proj.path())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(err(&o).contains("sub was not hidden"), "{}", err(&o));

    // Through a symlinked parent, turned off, then deleted: `on` still finds and removes it.
    let link = proj.path().join("link");
    std::os::unix::fs::symlink(proj.path(), &link).unwrap();
    let gone = link.join("gone");
    std::fs::create_dir(proj.path().join("gone")).unwrap();
    let gone = gone.to_str().unwrap();
    let o = a.run(&["folder", gone, "off"]);
    assert!(o.status.success(), "{}{}", out(&o), err(&o));
    std::fs::remove_dir(proj.path().join("gone")).unwrap();
    let o = a.run(&["folder", gone, "on"]);
    assert!(o.status.success(), "{}{}", out(&o), err(&o));
    let config = std::fs::read_to_string(a.state.path().join("config.toml")).unwrap();
    assert!(!config.to_lowercase().contains("/gone"), "{config}");
}

/// With the real install's layout (the default state dir under HOME), a command that starts
/// the daemon starts it from the canonical binary, not from the binary it runs as: during an
/// upgrade an old build's hook must not start an old daemon.
#[test]
fn the_daemon_starts_from_the_canonical_binary() {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join(".lightbulb/connect/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let canonical = bin.join("lightbulb");
    let marker = home.path().join("started");
    std::fs::write(
        &canonical,
        format!(
            "#!/bin/sh\necho canonical >> '{}'\nexec '{EXE}' \"$@\"\n",
            marker.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&canonical, std::fs::Permissions::from_mode(0o755)).unwrap();
    let o = Command::new(EXE)
        .arg("who")
        .env_remove("LIGHTBULB_CONNECT_HOME")
        .env("HOME", home.path())
        .env("LIGHTBULB_CONNECT_CLAUDE_HOME", home.path())
        .env("LIGHTBULB_CONNECT_NO_LAUNCHCTL", "1")
        .env("LIGHTBULB_CONNECT_DEBUG_NO_DEFAULT_RELAYS", "1")
        .env("LIGHTBULB_CONNECT_DEBUG_NO_TELEMETRY", "1")
        .env("LIGHTBULB_CONNECT_DIALOG", "")
        .env_remove("LIGHTBULB_CONNECT_HOOK_CALLER")
        .output()
        .unwrap();
    let sock = home.path().join(".lightbulb/connect/sock");
    if let Ok(mut s) = std::os::unix::net::UnixStream::connect(&sock) {
        use std::io::{Read, Write};
        let _ = writeln!(s, r#"{{"id":1,"op":"shutdown"}}"#);
        let _ = s.read(&mut [0; 64]);
    }
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "canonical\n");
}

/// `daemon --install` (and so `setup`) stops the daemon that runs now, and waits for it to
/// let go of the lock, before it loads the job: a daemon launchd did not start would hold it.
#[cfg(target_os = "macos")]
#[test]
fn install_stops_the_running_daemon_first() {
    let e = Env::new();
    assert!(e.run(&["who"]).status.success());
    let path = e.state.path().join("daemon.lock");
    let pid: i32 = std::fs::read_to_string(&path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let o = e.run(&["daemon", "--install"]);
    assert!(o.status.success(), "{}", err(&o));
    let free = std::fs::File::open(&path).unwrap().try_lock().is_ok();
    assert!(free, "the old daemon still holds the lock");
    // It drops the lock as its last step; its process ends just after.
    common::until(
        5,
        "the old daemon exits",
        || unsafe { libc::kill(pid, 0) } != 0,
    );
    assert_ne!(
        unsafe { libc::kill(pid, 0) },
        0,
        "the old daemon still runs"
    );
}

/// A daemon that holds the lock and does not let go (here this test, holding it) is
/// reported by the pid in the file, and no job is installed behind it.
#[cfg(target_os = "macos")]
#[test]
fn install_refuses_while_the_old_daemon_will_not_stop() {
    let e = Env::new();
    let path = e.state.path().join("daemon.lock");
    std::fs::write(&path, "4242\n").unwrap();
    let holder = std::fs::File::open(&path).unwrap();
    holder.try_lock().unwrap();
    let o = e.run(&["daemon", "--install"]);
    drop(holder);
    assert_eq!(o.status.code(), Some(1));
    let text = err(&o);
    assert!(
        text.starts_with(
            "The old background service did not stop (pid 4242). Run: kill 4242, then "
        ) && text.trim_end().ends_with(" setup again."),
        "{text}"
    );
    assert!(!out(&o).contains("Background service"), "{}", out(&o));
}

/// A pid left in the file by a daemon long gone, now some live process that holds no lock,
/// is no daemon: the install goes ahead at once.
#[cfg(target_os = "macos")]
#[test]
fn install_ignores_a_stale_pid_of_a_live_process() {
    let e = Env::new();
    let mut other = Command::new("/bin/sleep").arg("60").spawn().unwrap();
    std::fs::write(e.state.path().join("daemon.lock"), other.id().to_string()).unwrap();
    let t = std::time::Instant::now();
    let o = e.run(&["daemon", "--install"]);
    let alive = unsafe { libc::kill(other.id() as i32, 0) } == 0;
    let _ = other.kill();
    let _ = other.wait();
    assert!(o.status.success(), "{}", err(&o));
    assert!(
        t.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        t.elapsed()
    );
    assert!(alive, "the unrelated process was left alone");
}

#[test]
fn policy_refuses_your_own_person_and_forgets_an_old_row() {
    let e = Env::new();
    assert!(e.run(&["who"]).status.success());
    let me = e.ask(json!({"id": 1, "op": "status"}))["member_key_short"]
        .as_str()
        .unwrap()
        .to_string();
    let o = e.person(&["policy", &me, "hold"]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        err(&o).trim(),
        "That is you; your own computers always get through."
    );
    // A row an older build stored for my own person is shown as ignored.
    let store = lightbulb::store::Store::open(&e.state.path().join("state.db")).unwrap();
    let mine = e.ask(json!({"id": 1, "op": "team.get"}))["members"][0]["person_id"]
        .as_str()
        .unwrap()
        .to_string();
    lightbulb::policy::set_person(&store, &mine, lightbulb::policy::Verdict::Hold, "local", 1)
        .unwrap();
    let o = e.person(&["policy"]);
    assert!(
        out(&o).contains(
            ": held for your Allow. (ignored: your own computers always get through — run: "
        ) && out(&o).contains(&format!(" policy {mine} forget)")),
        "{}",
        out(&o)
    );
    // A row an older build stored under a literal name is marked, and forgotten on request.
    lightbulb::policy::set_person(&store, "aden", lightbulb::policy::Verdict::Hold, "local", 1)
        .unwrap();
    let o = e.person(&["policy"]);
    assert!(
        out(&o).contains("Messages from aden: held for your Allow. (old entry — run: ")
            && out(&o).contains(" policy aden forget)"),
        "{}",
        out(&o)
    );
    let o = e.person(&["policy", "aden", "forget"]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(!out(&o).contains("aden"), "{}", out(&o));
}

/// `daemon --install` over a loaded job boots it out BEFORE it stops the daemon: with the job
/// loaded, KeepAlive would restart the stopped daemon, which would retake the lock. A
/// `launchctl` shim on PATH records each call and whether the daemon's socket still answered
/// then; the first call must be the bootout, made while the old daemon still runs.
#[cfg(target_os = "macos")]
#[test]
fn install_boots_the_job_out_before_it_stops_the_daemon() {
    use std::os::unix::fs::PermissionsExt;
    let e = Env::new();
    assert!(e.run(&["who"]).status.success());
    let shim = tempfile::tempdir().unwrap();
    let log = shim.path().join("calls");
    let sock = e.state.path().join("sock");
    std::fs::write(
        shim.path().join("launchctl"),
        format!(
            "#!/bin/sh\nif [ -S '{}' ]; then up=live; else up=gone; fi\necho \"$1 $up\" >> '{}'\n",
            sock.display(),
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        shim.path().join("launchctl"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let path = format!("{}:/usr/bin:/bin", shim.path().display());
    let o = e
        .cmd(&["daemon", "--install"])
        .env_remove("LIGHTBULB_CONNECT_NO_LAUNCHCTL")
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", err(&o));
    let calls = std::fs::read_to_string(&log).unwrap();
    let first = calls.lines().next().unwrap_or_default();
    assert_eq!(first, "bootout live", "{calls}");
    assert!(
        calls.lines().any(|l| l.starts_with("bootstrap ")),
        "{calls}"
    );
}

#[test]
fn the_command_is_lightbulb() {
    let e = Env::new();
    let help = out(&e.run(&["--help"]));
    assert!(
        help.contains("\nUsage: lightbulb <command> [options]\n"),
        "{help}"
    );
    assert_eq!(
        out(&e.run(&["version"])).trim(),
        format!("lightbulb {}", env!("CARGO_PKG_VERSION"))
    );

    let h = e.home.path();
    // Any link there is replaced.
    let bin = h.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("/usr/bin/true", bin.join("lightbulb")).unwrap();
    let o = e.run(&["setup"]);
    assert!(o.status.success(), "{}", err(&o));
    let text = out(&o);
    assert!(text.starts_with("Command: lightbulb"), "{text}");
    assert!(
        text.contains("Add your other computer or a teammate: lightbulb invite\n"),
        "{text}"
    );
    let canon = h.join(".lightbulb/connect/bin/lightbulb");
    assert!(canon.is_file());
    assert_eq!(std::fs::read_link(bin.join("lightbulb")).unwrap(), canon);
}

/// A fresh Mac: `~/.local/bin` is not on PATH, so setup adds it to the zsh profile once (a
/// second run adds nothing) and says so, instead of asking the person to edit the file.
#[cfg(target_os = "macos")]
#[test]
fn daemon_install_adds_local_bin_to_the_zsh_profile_once() {
    let e = Env::new();
    let run = || {
        e.cmd(&["daemon", "--install"])
            .env("SHELL", "/bin/zsh")
            .env("PATH", "/usr/bin:/bin")
            .env("LIGHTBULB_CONNECT_DEBUG_TTY", "1")
            .output()
            .unwrap()
    };
    let o = run();
    assert!(o.status.success(), "{}", err(&o));
    assert!(
        out(&o).contains("added ~/.local/bin to your PATH in ~/.zshrc; open a new terminal"),
        "{}",
        out(&o)
    );
    let zshrc = e.home.path().join(".zshrc");
    let once = std::fs::read_to_string(&zshrc).unwrap();
    assert_eq!(
        once,
        "\n# Added by Lightbulb\nexport PATH=\"$HOME/.local/bin:$PATH\"\n"
    );
    assert!(run().status.success());
    assert_eq!(std::fs::read_to_string(&zshrc).unwrap(), once);
}

/// No person at a terminal (an update, the daemon's setup refresh, a pipe): the profile is not
/// touched, and setup says which line to add. Nor is a symlinked profile written through.
#[cfg(target_os = "macos")]
#[test]
fn setup_off_a_terminal_or_on_a_symlinked_profile_leaves_the_profile_alone() {
    let e = Env::new();
    let run = |tty: bool| {
        let mut c = e.cmd(&["daemon", "--install"]);
        c.env("SHELL", "/bin/zsh").env("PATH", "/usr/bin:/bin");
        if tty {
            c.env("LIGHTBULB_CONNECT_DEBUG_TTY", "1");
        }
        c.output().unwrap()
    };
    let advice = "Add this line to ~/.zshrc, then open a new terminal: export PATH=";
    let zshrc = e.home.path().join(".zshrc");
    let o = run(false);
    assert!(o.status.success(), "{}", err(&o));
    assert!(out(&o).contains(advice), "{}", out(&o));
    assert!(!zshrc.exists());
    let dotfiles = e.home.path().join("dotfiles-zshrc");
    std::fs::write(&dotfiles, "# mine\n").unwrap();
    std::os::unix::fs::symlink(&dotfiles, &zshrc).unwrap();
    let o = run(true);
    assert!(o.status.success(), "{}", err(&o));
    assert!(out(&o).contains(advice), "{}", out(&o));
    assert_eq!(std::fs::read_to_string(&dotfiles).unwrap(), "# mine\n");
}
