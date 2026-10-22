//! The team verbs against the built binary: `invite`, `remove`, `leave`, and the roster the
//! daemon keeps across restarts.

use lightbulb_network::identity::Identity;
use lightbulb_network::roster::{self, Entry, Kind};
use lightbulb_network::store::Store;
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

    fn run_with(&self, args: &[&str], stdin: &str) -> Output {
        let mut p = Command::new(env!("CARGO_BIN_EXE_lightbulb-network"))
            .args(args)
            .env("LIGHTBULB_NETWORK_HOME", self.state.path())
            .env("LIGHTBULB_NETWORK_CLAUDE_HOME", self.home.path())
            .env("LIGHTBULB_NETWORK_NO_LAUNCHCTL", "1")
            .env("HOME", self.home.path())
            .env("USER", "prasiddha")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        p.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        p.wait_with_output().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, "")
    }

    fn ask(&self, body: Value) -> Value {
        let mut s = UnixStream::connect(self.state.path().join("sock")).unwrap();
        writeln!(s, "{body}").unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn team_id(&self) -> String {
        self.ask(json!({"id": 1, "op": "status"}))["team"]["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Stops the daemon and waits until it has let go of its lock.
    fn stop(&self) {
        let mut s = UnixStream::connect(self.state.path().join("sock")).unwrap();
        writeln!(s, r#"{{"id":1,"op":"shutdown"}}"#).unwrap();
        let _ = s.read_to_end(&mut vec![]);
        let lock = std::fs::File::open(self.state.path().join("daemon.lock")).unwrap();
        lock.lock().unwrap();
    }

    fn entries(&self) -> Vec<Entry> {
        roster::entries(&Store::open(&self.state.path().join("state.db")).unwrap()).unwrap()
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

const LINE: &str = "Join the Lightbulb network: https://thelightbulb.company/j/#";

#[test]
fn invite_reprints_the_same_link_until_new() {
    let e = Env::new();
    let first = out(&e.run(&["invite"]));
    assert!(first.starts_with(LINE), "{first}");
    assert_eq!(out(&e.run(&["invite"])), first);
    let new = out(&e.run(&["invite", "--new"]));
    let line = new
        .strip_prefix("Old link stopped working. New link: ")
        .unwrap_or_else(|| panic!("{new}"));
    assert!(line.starts_with(LINE) && line != first, "{new}");
    assert_eq!(out(&e.run(&["invite"])), line);
    let once = out(&e.run(&["invite", "--once"]));
    assert!(once.starts_with("One-time invite, expires "), "{once}");
    assert!(once.contains(&format!(" UTC: {LINE}")), "{once}");
    // The once link leaves the reusable one alone.
    assert_eq!(out(&e.run(&["invite"])), line);
}

#[test]
fn non_admin_remove_prints_exact_text() {
    // A team founded elsewhere by "prasiddha", which admitted this computer as a member.
    let e = Env::new();
    let me = Identity::load_or_create(e.state.path()).unwrap();
    let founder = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
    let store = Store::open(&e.state.path().join("state.db")).unwrap();
    let g = Entry::signed(
        &founder,
        vec![],
        Kind::Admit,
        json!({"member": founder.key, "name": "prasiddha", "computer": "mac",
               "person_id": founder.key, "kind": "computer", "role": "founder",
               "ticket_id": "", "team_name": "acme"}),
        1,
    );
    let a = Entry::signed(
        &founder,
        vec![g.hash()],
        Kind::Admit,
        json!({"member": me.key, "name": "aden", "computer": "mbp", "person_id": me.key,
               "kind": "computer", "role": "member"}),
        2,
    );
    roster::accept(&store, &g, 10).unwrap();
    roster::accept(&store, &a, 10).unwrap();
    drop(store);

    let o = e.run(&["remove", "prasiddha"]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        err(&o).trim(),
        "Only the founder and admins can remove computers. Ask prasiddha."
    );
    let team = out(&e.run(&["team"]));
    assert!(team.contains("acme") && team.contains("aden"), "{team}");
    assert_eq!(e.entries().len(), 2, "no genesis over a joined team");
}

#[test]
fn restart_keeps_the_team_and_writes_no_second_genesis() {
    let e = Env::new();
    assert!(e.run(&["team"]).status.success());
    let id = e.team_id();
    assert_eq!(id.len(), 64);
    e.stop();
    assert!(e.run(&["team"]).status.success());
    assert_eq!(e.team_id(), id);
    assert_eq!(e.entries().len(), 1);
}

#[test]
fn leave_locally_recreates_a_solo_network() {
    let e = Env::new();
    let link = out(&e.run(&["invite"]));
    let id = e.team_id();
    let no = e.run_with(&["leave"], "n\n");
    assert!(no.status.success());
    assert_eq!(e.team_id(), id);

    let o = e.run_with(&["leave"], "y\n");
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(
        out(&o),
        "Leave \"prasiddha\"? Your computer stops connecting. Mail already stored here stays. [y/N] \
         Left \"prasiddha\". This computer is on its own network again.\n"
    );
    assert_ne!(e.team_id(), id);
    assert_eq!(e.entries().len(), 1);
    let links: i64 = rusqlite::Connection::open(e.state.path().join("state.db"))
        .unwrap()
        .query_row("SELECT count(*) FROM team_link", [], |r| r.get(0))
        .unwrap();
    assert_eq!(links, 0);
    let fresh = out(&e.run(&["invite"]));
    assert!(fresh.starts_with(LINE) && fresh != link);
}

#[test]
fn founder_admin_remove_and_leave_texts() {
    // This computer founded "acme" and admitted aden.
    let e = Env::new();
    let me = Identity::load_or_create(e.state.path()).unwrap();
    let aden = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
    let store = Store::open(&e.state.path().join("state.db")).unwrap();
    let g = Entry::signed(
        &me,
        vec![],
        Kind::Admit,
        json!({"member": me.key, "name": "prasiddha", "computer": "mac", "person_id": me.key,
               "kind": "computer", "role": "founder", "ticket_id": "", "team_name": "acme"}),
        1,
    );
    let a = Entry::signed(
        &me,
        vec![g.hash()],
        Kind::Admit,
        json!({"member": aden.key, "name": "aden", "computer": "mbp", "person_id": aden.key,
               "kind": "computer", "role": "member"}),
        2,
    );
    roster::accept(&store, &g, 10).unwrap();
    roster::accept(&store, &a, 10).unwrap();
    drop(store);

    let o = e.run_with(&["leave"], "y\n");
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        err(&o).trim(),
        "You are the founder. Name a new founder first: lightbulb-network admin <name> --founder"
    );
    assert_eq!(
        out(&e.run(&["admin", "ADEN"])),
        "aden can now remove members.\n"
    );
    let old = out(&e.run(&["invite"]));
    let o = out(&e.run(&["remove", "aden"]));
    let line = o
        .strip_prefix(
            "Removed aden. New connections and new mail from it are blocked. The invite link was replaced: ",
        )
        .unwrap_or_else(|| panic!("{o}"));
    assert!(line.starts_with(LINE) && line != old, "{o}");
    assert_eq!(out(&e.run(&["invite"])), line);
    assert!(!out(&e.run(&["team"])).contains("aden"));
}
