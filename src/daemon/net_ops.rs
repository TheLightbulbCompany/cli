//! The network ops, reached from `ipc::handle`'s fallback arm.

use super::ipc::{Response, UNKNOWN_OP};
use super::Daemon;
use crate::agents::caller;
use crate::os;
use serde_json::{json, Value};

pub fn handle(
    id: u64,
    op: &str,
    _body: &Value,
    daemon: &Daemon,
    _peer_pid: Option<u32>,
) -> Response {
    match op {
        // `identity.get {}` -> who this computer is. Until sign-in, `person_id` is its own key.
        "identity.get" => Response::ok(
            id,
            json!({
                "member_key": daemon.identity.key,
                "member_key_short": daemon.identity.short,
                "name": super::user_name(),
                "computer": daemon.team.lock().unwrap().config.computer,
                "person_id": daemon.identity.key,
                "verified": false,
            }),
        ),
        _ => Response::err(id, UNKNOWN_OP),
    }
}

/// Whether a person, not an agent, is at the other end of the socket: no process from
/// `peer_pid` up to pid 1 is an agent. Fails closed: a lookup that fails is no person.
/// In debug builds, the process `CALLER_ENV` names counts as an agent.
// ponytail: same-uid ceiling as `caller::walked_to_a_person` (a detached agent passes).
pub fn person_only(peer_pid: u32) -> bool {
    no_agent(caller::ancestors_from(peer_pid))
}

fn no_agent(
    mut walk: impl Iterator<Item = Result<(os::Process, bool), caller::CallerError>>,
) -> bool {
    walk.all(|step| step.is_ok_and(|(_, agent)| !agent))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    /// Spawns `exe args`; returns it and the first stdout line that is a pid.
    fn child_of(exe: &std::path::Path, args: &[&str]) -> (std::process::Child, u32) {
        let mut p = Command::new(exe)
            .args(args)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = BufReader::new(p.stdout.take().unwrap())
            .lines()
            .find_map(|l| l.ok()?.trim().parse().ok())
            .unwrap();
        (p, pid)
    }
    fn kill(pid: u32) {
        // SAFETY: a plain signal to a pid this test started.
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }

    #[test]
    fn person_only_accepts_people_and_refuses_agents() {
        let person_walk = |pid, pin| no_agent(caller::ancestors_pinned(pid, pin));
        assert!(person_only(1));
        assert!(!person_walk(999_999, None));

        // No agent above: a `sleep` reparented to pid 1 once its `sh` exits.
        // ponytail: the test's own ancestry may hold one (run under Claude Code), so detach.
        let (mut sh, sleep) = child_of("/bin/sh".as_ref(), &["-c", "sleep 30 & echo $!"]);
        sh.wait().unwrap();
        assert!(person_walk(sleep, None));
        // The pinned process counts as an agent; a stale pin (another birth) does not.
        let pin = os::process(sleep).unwrap();
        assert!(!person_walk(sleep, Some(pin.clone())));
        let stale = os::Process {
            birth_micros: pin.birth_micros + 1,
            ..pin
        };
        assert!(person_walk(sleep, Some(stale)));
        kill(sleep);

        // An agent above: this test binary installed as `claude` (a copied system binary is
        // killed on launch), running `sleep_under_me`, so `sleep`'s parent is `claude`.
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("claude");
        std::fs::copy(std::env::current_exe().unwrap(), &agent).unwrap();
        let (mut claude, child) = child_of(
            &agent,
            &[
                "--exact",
                "daemon::net_ops::tests::sleep_under_me",
                "--ignored",
                "--nocapture",
            ],
        );
        // Refused for the agent above it, not for a failed lookup.
        let parent = os::parent(child).unwrap().executable;
        assert_eq!(caller::classify(&parent), Some("claude"));
        assert!(!person_walk(child, None));
        kill(child);
        claude.wait().unwrap();
    }

    #[test]
    #[ignore = "a helper the test above runs as an agent"]
    fn sleep_under_me() {
        let mut sleep = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        println!("{}", sleep.id());
        sleep.wait().unwrap();
    }
}
