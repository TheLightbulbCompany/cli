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
/// In debug builds, `CALLER_ENV` set means an agent is the caller.
// ponytail: same-uid ceiling as `caller::walked_to_a_person` (a detached agent passes).
pub fn person_only(peer_pid: u32) -> bool {
    #[cfg(debug_assertions)]
    if std::env::var_os(crate::hook::CALLER_ENV).is_some() {
        return false;
    }
    // Pid 1 (launchd, which a non-root lookup cannot read) is no agent.
    let mut pid = peer_pid;
    while pid > 1 {
        let Ok(p) = os::process(pid) else {
            return false;
        };
        if caller::classify(&p.executable).is_some() {
            return false;
        }
        let Ok(up) = os::parent_pid(pid) else {
            return false;
        };
        pid = up;
    }
    true
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn person_only_fails_closed() {
        assert!(super::person_only(1));
        assert!(!super::person_only(999_999));
        // A process whose ancestry holds no agent: `sleep`, reparented to pid 1 once its
        // `sh` exits. ponytail: the test's own ancestry may hold one (run under Claude Code).
        let mut sh = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 5 & echo $!"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(sh.stdout.take().unwrap()),
            &mut line,
        )
        .unwrap();
        sh.wait().unwrap();
        let sleep: u32 = line.trim().parse().unwrap();
        assert!(super::person_only(sleep));
        // SAFETY: a plain signal to the pid this test started.
        unsafe { libc::kill(sleep as i32, libc::SIGKILL) };
    }
}
