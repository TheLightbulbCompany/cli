//! The team verbs against the built binary: `invite`, `remove`, `leave`, and the roster the
//! daemon keeps across restarts.

use lightbulb::daemon::net_ops;
use lightbulb::identity::Identity;
use lightbulb::os;
use lightbulb::roster::{self, Entry, Kind};
use lightbulb::store::Store;
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

const LINE: &str = "lightbulb join '";
const HEADER: &str = "Send this line to the person (or paste it on your other computer):\n";

/// What `invite` printed before its header (a notice, or nothing), and the join command
/// after it; the app link follows on its own line, for the same ticket.
fn invite_parts(o: &std::process::Output) -> (String, String) {
    let text = out(o);
    let (notice, rest) = text.split_once(HEADER).unwrap_or_else(|| panic!("{text}"));
    let lines: Vec<&str> = rest.lines().collect();
    let [line, "Replace NAME with the name they go by.", app] = lines[..] else {
        panic!("{text}")
    };
    let ticket = line
        .strip_prefix(LINE)
        .and_then(|t| t.strip_suffix("' --name NAME"))
        .unwrap_or_else(|| panic!("{text}"));
    assert_eq!(app, format!("lightbulb://join/{ticket}"), "{text}");
    (notice.to_string(), line.to_string())
}

/// The join command `invite` printed.
fn printed_link(o: &std::process::Output) -> String {
    invite_parts(o).1
}

#[test]
fn invite_reprints_the_same_link_until_new() {
    let e = Env::new();
    let first = printed_link(&e.run(&["invite"]));
    assert!(first.starts_with(LINE), "{first}");
    assert_eq!(printed_link(&e.run(&["invite"])), first);
    let (notice, line) = invite_parts(&e.run(&["invite", "--new"]));
    assert_eq!(
        notice,
        "Every earlier invite from this computer stopped working.\n"
    );
    assert!(line != first, "{line}");
    assert_eq!(printed_link(&e.run(&["invite"])), line);
    let (once, _) = invite_parts(&e.run(&["invite", "--once"]));
    assert!(once.starts_with("One-time invite, expires "), "{once}");
    assert!(once.ends_with(" UTC.\n"), "{once}");
    // The once link leaves the reusable one alone.
    assert_eq!(printed_link(&e.run(&["invite"])), line);
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
    assert!(
        team.starts_with("Team \"acme\" — 2 computers\n  You ("),
        "{team}"
    );
    assert!(team.contains("  aden · "), "{team}");
    assert!(
        team.ends_with("Invite someone: lightbulb invite\n"),
        "{team}"
    );
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
    let link = printed_link(&e.run(&["invite"]));
    let id = e.team_id();
    let no = e.detached(&[EXE, "leave"], "n\n");
    assert!(no.status.success());
    assert_eq!(e.team_id(), id);

    let o = e.detached(&[EXE, "leave"], "y\n");
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(
        out(&o),
        "Leave \"prasiddha\"? Your computer stops connecting. Messages already stored here stay. [y/N] \
         Left \"prasiddha\". This computer is on its own network again.\n"
    );
    assert_ne!(e.team_id(), id);
    assert_eq!(e.entries().len(), 1);
    let links: i64 = rusqlite::Connection::open(e.state.path().join("state.db"))
        .unwrap()
        .query_row("SELECT count(*) FROM team_link", [], |r| r.get(0))
        .unwrap();
    assert_eq!(links, 0);
    let fresh = printed_link(&e.run(&["invite"]));
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
        "You are the founder. Name a new founder first: lightbulb admin <name> --founder"
    );
    assert_eq!(
        out(&e.run(&["admin", "ADEN"])),
        "aden can now remove members.\n"
    );
    let old = printed_link(&e.run(&["invite"]));
    let o = out(&e.run(&["remove", "aden"]));
    let line = o
        .strip_prefix(
            "Removed aden. New connections and new messages from it are blocked. The old invite stopped working; send this line instead: ",
        )
        .unwrap_or_else(|| panic!("{o}"))
        .trim_end();
    assert!(line.starts_with(LINE) && line != old, "{o}");
    assert_eq!(printed_link(&e.run(&["invite"])), line);
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
               "proof": common::issuer::workspace_proof(&bridge.key, "W1", "bridge", &[], lightbulb::daemon::ipc::now() as i64)}),
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

#[test]
fn policy_takes_a_teammates_name_and_lists_names() {
    let e = Env::new();
    e.seed(false);
    let o = e.run(&["policy", "aden", "hold"]);
    assert!(o.status.success(), "{}", err(&o));
    let team = e.ask(json!({"id": 1, "op": "team.get"}));
    let aden = team["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "aden")
        .unwrap()
        .clone();
    let line = format!(
        "Messages from aden ({}): held for your Allow.",
        aden["short"].as_str().unwrap()
    );
    assert!(out(&o).contains(&line), "{}", out(&o));
    // Stored under aden's person id, the one `decide` looks up.
    let p = e.ask(json!({"id": 1, "op": "policy.get"}));
    assert_eq!(
        p["people"][aden["person_id"].as_str().unwrap()],
        "hold",
        "{p}"
    );
    // By its short too, in any case for the name.
    assert!(e.run(&["policy", "ADEN", "accept"]).status.success());
    let short = aden["short"].as_str().unwrap();
    assert!(e.run(&["policy", short, "refuse"]).status.success());
    let o = e.run(&["policy", "nobody", "hold"]);
    assert_eq!(o.status.code(), Some(1));
    let e2 = err(&o);
    assert!(
        e2.starts_with("No teammate called \"nobody\". Teammates: ")
            && e2.contains("prasiddha")
            && e2.contains("aden")
            && e2.trim_end().ends_with("(see lightbulb team)."),
        "{e2}"
    );
    // A workspace id stays as given; any other id no member has is refused by the daemon.
    assert!(e.run(&["policy", "ws:w1", "accept"]).status.success());
    let r = common::person(
        &e,
        json!({"id": 1, "op": "policy.set", "person_id": "aden", "value": "hold"}),
    );
    assert_eq!(r["error"], "not_found", "{r}");
}

#[test]
fn a_replaced_identity_key_refuses_to_start_until_leave() {
    let (a, b) = (Env::new(), Env::new());
    common::seed_pair(&a, &b);
    // A new identity.key under the kept state.db: the team record is for the old one.
    std::fs::remove_file(a.state.path().join("identity.key")).unwrap();
    let line = "identity.key does not match this computer's team record. Run lightbulb leave, then join again with a new invite.\n";
    let run = |args: &[&str]| {
        let mut c = a.base_cmd(std::process::Command::new(EXE));
        c.args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    };
    let o = run(&["daemon"]);
    assert_eq!((o.status.code(), err(&o)), (Some(1), line.to_string()));
    // Any command says why, not just that the daemon did not start.
    let o = run(&["team"]);
    assert_eq!((o.status.code(), err(&o)), (Some(1), line.to_string()));
    // `leave` works without the daemon; then this computer starts on its own network.
    let o = a.detached(&[EXE, "leave"], "y\n");
    assert!(o.status.success(), "{}{}", out(&o), err(&o));
    assert!(out(&o).ends_with("Left \"acme\". This computer is on its own network again.\n"));
    a.daemon(false, "", &[]);
    let t = a.ask(json!({"id": 1, "op": "team.get"}));
    assert_eq!(t["members"].as_array().unwrap().len(), 1, "{t}");
}
