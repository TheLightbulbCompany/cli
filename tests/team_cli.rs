//! The team verbs against the built binary: `invite`, `remove`, `leave`, and the roster the
//! daemon keeps across restarts.

use lightbulb_connect::daemon::net_ops;
use lightbulb_connect::identity::Identity;
use lightbulb_connect::os;
use lightbulb_connect::roster::{self, Entry, Kind};
use lightbulb_connect::store::Store;
use serde_json::json;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::Output;

mod common;

use common::{err, out, Env, EXE};

impl Env {
    /// A daemon that takes this test process for an agent.
    fn pinned() -> Env {
        let birth = os::process(std::process::id()).unwrap().birth_micros;
        let mut e = Env::new();
        e.pin = Some(format!("{}:{birth}", std::process::id()));
        e
    }

    /// Seeds a two-member team "acme" before the daemon starts: this computer and aden's,
    /// founded by this computer or (`member`) by aden, admitting this one as "me".
    fn seed(&self, member: bool) {
        let me = Identity::load_or_create(self.state.path()).unwrap();
        let aden = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
        let (f, m, f_name, m_name) = if member {
            (&aden, &me, "aden", "me")
        } else {
            (&me, &aden, "prasiddha", "aden")
        };
        let g = net_ops::genesis(f, f_name, "mac", "acme", 1);
        let a = Entry::signed(
            f,
            vec![g.hash()],
            Kind::Admit,
            json!({"member": m.key, "name": m_name, "computer": "mbp", "person_id": m.key,
                   "kind": "computer", "role": "member"}),
            2,
        );
        let store = Store::open(&self.state.path().join("state.db")).unwrap();
        roster::accept(&store, &g, 10).unwrap();
        roster::accept(&store, &a, 10).unwrap();
    }

    /// The binary with `args`, run by a person (`detached`).
    fn run(&self, args: &[&str]) -> Output {
        self.detached(&[[EXE].as_slice(), args].concat(), "")
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

const LINE: &str = "Join Lightbulb Connect: https://thelightbulb.company/j/#";

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
    // A team founded elsewhere by aden, which admitted this computer as a plain member.
    let e = Env::new();
    e.seed(true);
    let o = e.run(&["remove", "aden"]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        err(&o).trim(),
        "Only the founder and admins can remove computers. Ask aden."
    );
    let team = out(&e.run(&["team"]));
    assert!(team.contains("acme") && team.contains("aden"), "{team}");
    assert_eq!(e.entries().len(), 2, "no genesis over a joined team");
    // An open link is for the founder and admins.
    let r = common::person(&e, json!({"id": 1, "op": "team.invite", "mode": "open"}));
    assert_eq!(r["error"], "not_admin", "{r}");
    assert_eq!(e.entries().len(), 2);
}

#[test]
fn remove_admin_and_leave_from_an_agent_shell_are_refused() {
    let e = Env::pinned();
    e.seed(false);
    assert!(e.run(&["team"]).status.success()); // starts the pinned daemon
    let before = e.entries();
    for req in [
        json!({"op": "team.remove", "member": "aden"}),
        json!({"op": "team.admin", "member": "aden", "founder": false}),
        json!({"op": "team.admin", "member": "aden", "founder": true}),
        json!({"op": "team.leave"}),
        json!({"op": "team.invite", "mode": "open"}),
    ] {
        let mut req = req;
        req["id"] = 1.into();
        let r = e.ask(req.clone());
        assert_eq!(
            (r["error"].as_str(), r["reason"].as_str()),
            (Some("bad_request"), Some("not_a_person")),
            "{req}"
        );
    }
    assert_eq!(e.entries(), before);
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
    let no = e.detached(&[EXE, "leave"], "n\n");
    assert!(no.status.success());
    assert_eq!(e.team_id(), id);

    let o = e.detached(&[EXE, "leave"], "y\n");
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
    e.seed(false);

    let o = e.detached(&[EXE, "leave"], "y\n");
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        err(&o).trim(),
        "You are the founder. Name a new founder first: lightbulb-connect admin <name> --founder"
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

#[test]
fn founder_whose_only_other_member_is_cloud_can_leave() {
    let e = Env::new();
    assert!(e.run(&["team"]).status.success());
    e.stop();
    let me = Identity::load_or_create(e.state.path()).unwrap();
    let bridge = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
    let g = e.entries().remove(0);
    let a = Entry::signed(
        &me,
        vec![g.hash()],
        Kind::Admit,
        json!({"member": bridge.key, "name": "bridge", "computer": "cloud", "person_id": "ws:W1",
               "kind": "cloud", "role": "member",
               "proof": common::issuer::workspace_proof(&bridge.key, "W1", "bridge", &[], common::now_s())}),
        2,
    );
    roster::accept(
        &Store::open(&e.state.path().join("state.db")).unwrap(),
        &a,
        10,
    )
    .unwrap();
    // A cloud member can be neither admin nor founder.
    let o = e.run(&["admin", "bridge"]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    let o = e.detached(&[EXE, "leave"], "y\n");
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(e.entries().len(), 1);
}

#[test]
fn a_computer_outside_the_roster_is_told_so() {
    // A team aden founded that never admitted this computer.
    let e = Env::new();
    let aden = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
    let g = net_ops::genesis(&aden, "aden", "mac", "acme", 1);
    let store = Store::open(&e.state.path().join("state.db")).unwrap();
    roster::accept(&store, &g, 10).unwrap();
    let o = e.run(&["remove", "aden"]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(err(&o).trim(), "This computer is not in the network.");
}
