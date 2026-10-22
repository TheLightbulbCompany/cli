//! The network ops, reached from `ipc::handle`'s fallback arm.

use super::ipc::{refused, Response, BAD_REQUEST, INTERNAL, NOT_FOUND, UNKNOWN_OP};
use super::{Daemon, TeamState};
use crate::agents::caller;
use crate::identity::Identity;
use crate::roster::{self, Entry, Kind, MemberKind, Role, Roster};
use crate::store::Store;
use crate::{os, ticket};
use rusqlite::params;
use serde_json::{json, Value};

pub const NOT_ADMIN: &str = "not_admin";
pub const NOT_FOUNDER: &str = "not_founder";
pub const NOT_MEMBER: &str = "not_member";
pub const FOUNDER_MUST_TRANSFER: &str = "founder_must_transfer";

pub fn handle(id: u64, op: &str, body: &Value, daemon: &Daemon, peer_pid: Option<u32>) -> Response {
    if op.starts_with("team.") {
        // Governance and open links need a person at the socket, before any authority check.
        let governs = matches!(op, "team.remove" | "team.admin" | "team.leave")
            || (op == "team.invite" && body["mode"] == "open");
        if governs && !peer_pid.is_some_and(person_only) {
            return refused(id, "not_a_person");
        }
        let mut team = daemon.team.lock().unwrap();
        let store = daemon.store.lock().unwrap();
        let ops = TeamOps {
            id,
            daemon,
            team: &mut team,
            store: &store,
            now: super::ipc::now() as i64,
        };
        return match ops.run(op, body) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{op}: {e}");
                Response::err(id, INTERNAL)
            }
        };
    }
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

/// The genesis of a solo network: this computer, the founder of a team called `team_name`.
/// Stamped after `after` so a new network never repeats an old one's `team_id`.
pub(super) fn genesis(
    store: &Store,
    me: &Identity,
    computer: &str,
    team_name: &str,
    now: i64,
    after: i64,
) -> crate::Result<()> {
    let body = json!({"member": me.key, "name": super::user_name(), "computer": computer,
        "person_id": me.key, "kind": "computer", "role": "founder", "ticket_id": "",
        "team_name": team_name});
    let e = Entry::signed(me, vec![], Kind::Admit, body, now.max(after + 1));
    roster::accept(store, &e, now)?;
    Ok(())
}

/// The stored roster, replayed into `team` and Plan A's `team` row.
pub(super) fn reload(store: &Store, team: &mut TeamState, now: i64) -> crate::Result<Roster> {
    let r = roster::replay(&roster::entries(store)?);
    let founder = r.members.values().find(|m| m.role == Role::Founder);
    store.set_team(
        &r.team_id,
        &r.team_name,
        founder.map_or("", |m| m.key.as_str()),
        now,
    )?;
    team.team_id = r.team_id.clone();
    team.team_name = r.team_name.clone();
    team.heads = r.heads.clone();
    Ok(r)
}

/// One team op, under the `team` then `store` locks.
struct TeamOps<'a> {
    id: u64,
    daemon: &'a Daemon,
    team: &'a mut TeamState,
    store: &'a Store,
    now: i64,
}

impl TeamOps<'_> {
    fn run(mut self, op: &str, body: &Value) -> crate::Result<Response> {
        let id = self.id;
        let r = roster::replay(&roster::entries(self.store)?);
        let me_key = self.daemon.identity.key.clone();
        // `None` once this computer is no longer in the roster (removed by someone else).
        let role = r.members.get(&me_key).map(|m| m.role);
        let target = || {
            let who = body["member"].as_str()?;
            r.members.get(who).cloned().or_else(|| {
                let who = who.to_lowercase();
                r.members
                    .values()
                    .find(|m| m.name.to_lowercase() == who)
                    .cloned()
            })
        };
        Ok(match op {
            // `team.get {}` -> `{team_id, name, members, heads}`.
            "team.get" => Response::ok(
                id,
                json!({"team_id": r.team_id, "name": r.team_name,
                    "members": r.members.values().collect::<Vec<_>>(), "heads": r.heads}),
            ),
            // `team.invite {mode}` -> `{ticket, line, app_link, expires_at, replaced}`. `reusable`
            // (and `open`) reprint this member's live link of that mode, minting one if none.
            "team.invite" => {
                let mode = body["mode"].as_str().unwrap_or("reusable");
                if !matches!(mode, "reusable" | "new" | "once" | "open") {
                    return Ok(Response::err(id, BAD_REQUEST));
                }
                let open = mode == "open";
                if role.is_none() {
                    return Ok(Response::err(id, NOT_MEMBER));
                }
                if open && role == Some(Role::Member) {
                    return Ok(Response::err(id, NOT_ADMIN));
                }
                let link_mode = if open { "open" } else { "ask" };
                let live = r.links.values().find(|l| {
                    l.signer == me_key && !l.once && l.mode == link_mode && l.expires_at > self.now
                });
                let t = match live {
                    Some(l) if mode == "reusable" || open => ticket::sign(
                        &self.daemon.identity,
                        &r,
                        l.ticket_id.clone(),
                        l.secret.clone(),
                        link_mode,
                        l.expires_at,
                    ),
                    _ => self.new_link(&r, link_mode, mode == "once")?,
                };
                Response::ok(
                    id,
                    json!({"ticket": ticket::encode(&t), "line": ticket::line(&t),
                        "app_link": ticket::app_link(&t), "expires_at": t.expires_at,
                        "replaced": mode == "new" && live.is_some()}),
                )
            }
            // `team.admin {member, founder}` -> `{member, name}`: an admin, or (`founder`) the
            // founder in our place.
            "team.admin" => {
                if role != Some(Role::Founder) {
                    return Ok(Response::err(id, NOT_FOUNDER));
                }
                let Some(t) = target() else {
                    return Ok(Response::err(id, NOT_FOUND));
                };
                // A cloud member never governs.
                if t.key == me_key || t.kind == MemberKind::Cloud {
                    return Ok(Response::err(id, BAD_REQUEST));
                }
                let kind = if body["founder"] == true {
                    Kind::Transfer
                } else {
                    Kind::Admin
                };
                self.write(kind, json!({"member": t.key}))?;
                Response::ok(id, json!({"member": t.key, "name": t.name}))
            }
            // `team.remove {member}` -> `{name, line}`: the removal ends every live link, so a
            // new one follows it.
            "team.remove" => {
                if role.is_none_or(|r| r == Role::Member) {
                    let founder = r.members.values().find(|m| m.role == Role::Founder);
                    return Ok(Response {
                        id,
                        ok: false,
                        body: json!({"error": NOT_ADMIN, "founder": founder.map(|m| &m.name)}),
                    });
                }
                let Some(t) = target() else {
                    return Ok(Response::err(id, NOT_FOUND));
                };
                if t.key == me_key || t.role == Role::Founder {
                    return Ok(Response::err(id, BAD_REQUEST));
                }
                let (_, after) = self.write(Kind::Remove, json!({"member": t.key}))?;
                let link = self.new_link(&after, "ask", false)?;
                Response::ok(id, json!({"name": t.name, "line": ticket::line(&link)}))
            }
            // `team.leave {}` -> `{name}`: leaves, then starts this computer's solo network.
            "team.leave" => {
                // Cloud members (a person's bridge) do not keep the founder in.
                let others = r
                    .members
                    .values()
                    .any(|m| m.key != me_key && m.kind == MemberKind::Computer);
                let admin = r.members.values().any(|m| m.role == Role::Admin);
                if role == Some(Role::Founder) && others && !admin {
                    return Ok(Response::err(id, FOUNDER_MUST_TRANSFER));
                }
                // ponytail: kept in a local only; Task 8 sends it to peers before the wipe.
                let _leave = match role {
                    Some(_) => Some(self.write(Kind::Leave, json!({"member": me_key}))?.0),
                    None => None,
                };
                let root_at = roster::entries(self.store)?
                    .iter()
                    .find(|e| e.parents.is_empty())
                    .map_or(0, |e| e.at);
                self.store
                    .conn()
                    .execute_batch("DELETE FROM roster; DELETE FROM team_link;")?;
                genesis(
                    self.store,
                    &self.daemon.identity,
                    &self.team.config.computer,
                    &super::user_name(),
                    self.now,
                    root_at,
                )?;
                reload(self.store, self.team, self.now)?;
                Response::ok(id, json!({"name": r.team_name}))
            }
            _ => Response::err(id, UNKNOWN_OP),
        })
    }

    /// Signs `kind` over the current heads and applies it: the entry and the roster after it.
    fn write(&mut self, kind: Kind, body: Value) -> crate::Result<(Entry, Roster)> {
        let e = Entry::signed(
            &self.daemon.identity,
            self.team.heads.clone(),
            kind,
            body,
            self.now,
        );
        roster::accept(self.store, &e, self.now)?;
        Ok((e, reload(self.store, self.team, self.now)?))
    }

    /// Mints a link of `mode` (signed into the roster, recorded in `team_link`): its ticket.
    fn new_link(&mut self, r: &Roster, mode: &str, once: bool) -> crate::Result<ticket::Ticket> {
        let (t, hashed) = ticket::mint(&self.daemon.identity, r, mode, once, self.now);
        self.write(Kind::Link, ticket::link_body(&t, &hashed))?;
        // A once link's secret is kept only as its hash.
        self.store.conn().execute(
            "INSERT INTO team_link VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
            params![
                t.ticket_id,
                hashed.is_none().then_some(&t.secret),
                hashed,
                mode,
                once,
                t.expires_at
            ],
        )?;
        Ok(t)
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
