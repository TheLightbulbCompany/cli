//! The network ops, reached from `ipc::handle`'s fallback arm.

use super::ipc::{refused, Response, BAD_REQUEST, INTERNAL, NOT_FOUND, UNKNOWN_OP, UNSUPPORTED};
use super::{Daemon, TeamState};
use crate::agents::caller;
use crate::identity::Identity;
use crate::net::frames::Frame;
use crate::roster::{self, Entry, Kind, MemberKind, Role, Roster};
use crate::store::Store;
use crate::{os, ticket};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Duration;

pub const NOT_ADMIN: &str = "not_admin";
pub const NOT_FOUNDER: &str = "not_founder";
pub const NOT_MEMBER: &str = "not_member";
pub const FOUNDER_MUST_TRANSFER: &str = "founder_must_transfer";

/// `team.invite`'s `mode`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Invite {
    /// This member's live link, minted when it has none.
    #[default]
    Reusable,
    /// A new link in place of the live one.
    New,
    /// A link for one join.
    Once,
    /// A link whose joins need no Allow (founder and admins).
    Open,
}

impl Invite {
    fn link_mode(self) -> &'static str {
        if self == Invite::Open {
            "open"
        } else {
            "ask"
        }
    }

    fn once(self) -> bool {
        self == Invite::Once
    }
}

/// Who may run a team op: a person at the socket (not an agent), and at least `min_role`
/// (`None`: anyone, even a computer the team removed).
struct Gate {
    person_only: bool,
    min_role: Option<Role>,
}

fn gate(op: &str, invite: Invite) -> Gate {
    let (person_only, min_role) = match op {
        "team.remove" => (true, Some(Role::Admin)),
        "team.admin" => (true, Some(Role::Founder)),
        "team.admit" => (true, None),
        "team.invite" if invite == Invite::Open => (true, Some(Role::Admin)),
        "team.invite" => (false, Some(Role::Member)),
        _ => (false, None),
    };
    Gate {
        person_only,
        min_role,
    }
}

pub fn handle(id: u64, op: &str, body: &Value, daemon: &Daemon, peer_pid: Option<u32>) -> Response {
    // Retired: a cloud member is removed by an admin like any member.
    if op == "team.unlink" {
        return Response::err(id, UNSUPPORTED);
    }
    if op.starts_with("team.") {
        let invite = if op == "team.invite" {
            match serde_json::from_value::<Option<Invite>>(body["mode"].clone()) {
                Ok(i) => i.unwrap_or_default(),
                Err(_) => return Response::err(id, BAD_REQUEST),
            }
        } else {
            Invite::default()
        };
        let gate = gate(op, invite);
        // Governance and open links need a person at the socket, before any authority check.
        if gate.person_only && !peer_pid.is_some_and(person_only) {
            return refused(id, "not_a_person");
        }
        match op {
            "team.admit" => return crate::admission::admit_op(id, body, daemon),
            "team.join_requests" => return crate::admission::requests_op(id, daemon),
            _ => {}
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
        return match ops.run(op, body, invite, gate.min_role) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{op}: {e}");
                Response::err(id, INTERNAL)
            }
        };
    }
    match op {
        // `identity.get {}` -> who this computer is: until sign-in, `person_id` is its own key
        // and `name` the user's; `verified` while its proof is in force (`expires_at`).
        "identity.get" => {
            let t = daemon.team.lock().unwrap();
            let me = &daemon.identity.key;
            let proof = t.roster.members.get(me).and_then(|m| m.proof.as_ref());
            Response::ok(
                id,
                json!({
                    "member_key": me,
                    "member_key_short": daemon.identity.short,
                    "name": proof.map_or_else(super::user_name, |p| p.name.clone()),
                    "computer": t.config.computer,
                    "person_id": t.roster.person_of(me),
                    "verified": proof.is_some(),
                    "expires_at": proof.map(|p| p.exp),
                    "solo": t.roster.is_solo(me),
                }),
            )
        }
        // An agent fed a hostile sign-in must not move this computer into another person.
        "identity.proof" | "identity.device_start" if !peer_pid.is_some_and(person_only) => {
            refused(id, "not_a_person")
        }
        // `identity.device_start {}` -> `{member_key, computer, at, sig}`: `device/start`'s
        // body, `sig` by this computer's key over the rest (person-only).
        "identity.device_start" => {
            let computer = daemon.team.lock().unwrap().config.computer.clone();
            let at = super::ipc::now() as i64;
            Response::ok(
                id,
                crate::proof::start_body(&daemon.identity, &computer, at),
            )
        }
        // `identity.proof {proof}` -> `{name, person_id, solo}`: this computer's sign-in proof,
        // written to the roster (`proof::record`), person-only; `bad_request` when it does not
        // verify (reason `other_person` when it names another person than this computer's).
        "identity.proof" => match body["proof"]
            .as_str()
            .map(|t| crate::proof::record(daemon, t))
        {
            Some(Ok(p)) => {
                let solo = daemon
                    .team
                    .lock()
                    .unwrap()
                    .roster
                    .is_solo(&daemon.identity.key);
                Response::ok(
                    id,
                    json!({"name": p.name, "person_id": p.sub, "solo": solo}),
                )
            }
            Some(Err(e)) => {
                eprintln!("identity.proof: refused: {e}");
                if e.to_string() == crate::proof::OTHER_PERSON {
                    refused(id, "other_person")
                } else {
                    Response::err(id, BAD_REQUEST)
                }
            }
            None => Response::err(id, BAD_REQUEST),
        },
        // `ui.claim {}` -> `{lease_s}`: the Lightbulb app shows the person's questions itself;
        // no OS dialog opens until it stops heartbeating.
        "ui.claim" => {
            daemon.dialogs.claim(super::ipc::now() as i64);
            Response::ok(id, json!({ "lease_s": crate::dialog::LEASE_S }))
        }
        "reload" => match reload(daemon) {
            Ok(()) => Response::ok(id, json!({})),
            Err(e) => {
                eprintln!("reload: {e}");
                Response::err(id, INTERNAL)
            }
        },
        _ => Response::err(id, UNKNOWN_OP),
    }
}

/// `reload {}` -> `{}`: re-reads `config.toml` into `TeamState`, and rebuilds the presence (a
/// session under a folder turned off leaves it). Every file the daemon reads at start and may
/// read again is re-read here. Not person-only: `config.toml` is the agent's to write anyway.
pub fn reload(daemon: &Daemon) -> crate::Result<()> {
    let config = crate::config::Config::load(&crate::paths::state_dir())?;
    let off = super::canonical_off(&config);
    if daemon.team.lock().unwrap().set_config(config, off) {
        daemon.presence.notify_one();
    }
    Ok(())
}

/// How long `team.leave` waits for a peer to take the `leave` entry.
const TELL_WAIT: Duration = Duration::from_secs(5);

/// `team.leave {}` -> `{name, told}`: writes this computer's `leave` entry and sends it to the
/// online members, waiting up to 5 s for one to take it (it then drops the connection, as to
/// any computer no longer a member); `told` is false when none did. Then it forgets the team
/// and starts this computer's solo network. A founder with other computers in the team and
/// no admin must transfer first.
pub async fn leave(id: u64, daemon: &Daemon, peer_pid: Option<u32>) -> Response {
    if !peer_pid.is_some_and(person_only) {
        return refused(id, "not_a_person");
    }
    let me = daemon.identity.key.clone();
    let left = {
        let mut team = daemon.team.lock().unwrap();
        let store = daemon.store.lock().unwrap();
        let r = &team.roster;
        let role = r.members.get(&me).map(|m| m.role);
        // Cloud members (a person's bridge) do not keep the founder in.
        let others = r
            .members
            .values()
            .any(|m| m.key != me && m.kind == MemberKind::Computer);
        let admin = r.members.values().any(|m| m.role == Role::Admin);
        if role == Some(Role::Founder) && others && !admin {
            return Response::err(id, FOUNDER_MUST_TRANSFER);
        }
        // Alone (or already removed), there is no one to tell.
        let tell = role.is_some() && !r.is_solo(&me);
        let (name, root_at) = (r.team_name.clone(), r.created_at);
        let now = super::ipc::now() as i64;
        let leave = json!({"member": me});
        let entry = match role {
            Some(_) => write(daemon, &mut team, &store, Kind::Leave, leave, now).map(Some),
            None => Ok(None),
        };
        entry.map(|e| (name, root_at, e.filter(|_| tell)))
    };
    let (name, root_at, entry) = match left {
        Ok(l) => l,
        Err(e) => {
            eprintln!("team.leave: {e}");
            return Response::err(id, INTERNAL);
        }
    };
    let told = match entry {
        Some(e) => tell(daemon, e).await,
        None => true,
    };
    let mut team = daemon.team.lock().unwrap();
    let store = daemon.store.lock().unwrap();
    let mut solo = || -> crate::Result<()> {
        store.clear_roster()?;
        // Stamped after the old root, so the new network never repeats its `team_id`.
        let user = super::user_name();
        let now = super::ipc::now() as i64;
        let g = genesis(
            &daemon.identity,
            &user,
            &team.config.computer,
            &user,
            now.max(root_at + 1),
        );
        roster::accept(&store, &g, now)?;
        install(daemon, &mut team, &store, roster::replay(&[g]))
    };
    match solo() {
        Ok(()) => Response::ok(id, json!({"name": name, "told": told})),
        Err(e) => {
            eprintln!("team.leave: {e}");
            Response::err(id, INTERNAL)
        }
    }
}

/// Sends `leave` to every online member (and to any that comes online meanwhile) until one
/// of them drops its connection, as it does once it has applied it: `false` after
/// `TELL_WAIT` without that.
async fn tell(daemon: &Daemon, leave: Entry) -> bool {
    let frame = Frame::Roster {
        entries: vec![leave],
        more: false,
    };
    let deadline = tokio::time::Instant::now() + TELL_WAIT;
    let mut sent: HashSet<String> = HashSet::new();
    loop {
        // Armed before looking, so a change between the look and the wait is not missed.
        let gone = daemon.peers.disconnected.notified();
        let came = daemon.peers.connected.notified();
        tokio::pin!(gone, came);
        gone.as_mut().enable();
        came.as_mut().enable();
        let online: HashSet<String> = daemon.peers.online().into_iter().collect();
        if sent.iter().any(|k| !online.contains(k)) {
            return true;
        }
        for k in &online {
            if !sent.contains(k) && daemon.peers.send(k, frame.clone()).is_ok() {
                sent.insert(k.clone());
            }
        }
        tokio::select! {
            _ = &mut gone => {}
            _ = &mut came => {}
            _ = tokio::time::sleep_until(deadline) => return false,
        }
    }
}

/// The genesis of a solo network: `me`, called `name`, the founder of a team `team_name`.
pub fn genesis(me: &Identity, name: &str, computer: &str, team_name: &str, at: i64) -> Entry {
    let body = json!({"member": me.key, "name": name, "computer": computer,
        "person_id": me.key, "kind": "computer", "role": "founder", "ticket_id": "",
        "team_name": team_name});
    Entry::signed(me, vec![], Kind::Admit, body, at)
}

/// Rewrites Plan A's `team` row from `new` when its id, name or founder differs from `old`'s.
pub(super) fn sync_team_row(store: &Store, old: &Roster, new: &Roster) -> crate::Result<()> {
    fn row(r: &Roster) -> (&str, &str, &str) {
        let founder = r.founder().map_or("", |m| m.key.as_str());
        (&r.team_id, &r.team_name, founder)
    }
    let (id, name, founder) = row(new);
    if row(old) != (id, name, founder) {
        store.set_team(id, name, founder, new.created_at)?;
    }
    Ok(())
}

/// The one post-write hook, for local writes and entries from peers alike: makes `new` the
/// cached roster (its expired proofs lapsed), rewrites Plan A's `team` row if it changed, and tells `net` (which closes
/// the connections of members gone, dials new ones and pushes the change).
pub fn install(
    daemon: &Daemon,
    team: &mut TeamState,
    store: &Store,
    new: Roster,
) -> crate::Result<()> {
    sync_team_row(store, &team.roster, &new)?;
    let mut new = new;
    new.lapse(super::ipc::now() as i64);
    team.roster = new;
    daemon.peers.changed.notify_one();
    Ok(())
}

/// Signs `kind` over `team`'s heads and accepts it; the one replay after it becomes the
/// cached roster (`install`). Returns the entry. Under the `team` then `store` locks.
pub fn write(
    daemon: &Daemon,
    team: &mut TeamState,
    store: &Store,
    kind: Kind,
    body: Value,
    now: i64,
) -> crate::Result<Entry> {
    let e = Entry::signed(&daemon.identity, team.roster.heads.clone(), kind, body, now);
    roster::accept(store, &e, now)?;
    install(
        daemon,
        team,
        store,
        roster::replay(&roster::entries(store)?),
    )?;
    Ok(e)
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
    fn run(
        mut self,
        op: &str,
        body: &Value,
        invite: Invite,
        min_role: Option<Role>,
    ) -> crate::Result<Response> {
        let id = self.id;
        let identity = &self.daemon.identity;
        let me_key = identity.key.clone();
        let r = &self.team.roster;
        // `None` once this computer is no longer in the roster (removed by someone else).
        let role = r.members.get(&me_key).map(|m| m.role);
        // Membership first: a computer the team no longer holds is `not_member` for any op
        // with a minimum role.
        if min_role.is_some() && role.is_none() {
            return Ok(Response::err(id, NOT_MEMBER));
        }
        if role < min_role {
            return Ok(match min_role {
                Some(Role::Founder) => Response::err(id, NOT_FOUNDER),
                Some(Role::Admin) => Response::err_with(
                    id,
                    NOT_ADMIN,
                    json!({"founder": r.founder().map(|m| &m.name)}),
                ),
                _ => Response::err(id, NOT_MEMBER),
            });
        }
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
            // `team.get {}` -> `{team_id, name, members, heads}`, each member with `online`
            // (this computer always).
            "team.get" => {
                let online = self.daemon.peers.online();
                let members: Vec<Value> = r
                    .members
                    .values()
                    .map(|m| {
                        let mut v = json!(m);
                        v["online"] = (m.key == me_key || online.contains(&m.key)).into();
                        v
                    })
                    .collect();
                Response::ok(
                    id,
                    json!({"team_id": r.team_id, "name": r.team_name, "members": members,
                        "heads": r.heads}),
                )
            }
            // `team.invite {mode}` -> `{ticket, line, app_link, expires_at, replaced}`. `reusable`
            // (and `open`) reprint this member's live link of that mode, minting one if none.
            "team.invite" => {
                let mode = invite.link_mode();
                let live = r
                    .links
                    .values()
                    .find(|l| {
                        !invite.once()
                            && l.signer == me_key
                            && !l.once
                            && l.mode == mode
                            && l.expires_at > self.now
                    })
                    .cloned();
                let replaced = invite == Invite::New && live.is_some();
                let t = match live {
                    Some(l) if invite != Invite::New => {
                        ticket::sign(identity, r, l.ticket_id, l.secret, mode, l.expires_at)
                    }
                    _ => self.new_link(mode, invite.once())?,
                };
                let s = ticket::encode(&t);
                Response::ok(
                    id,
                    json!({"line": ticket::join_line(&s), "app_link": ticket::app_link(&s),
                        "ticket": s, "expires_at": t.expires_at, "replaced": replaced}),
                )
            }
            // `team.admin {member, founder}` -> `{member, name}`: an admin, or (`founder`) the
            // founder in our place.
            "team.admin" => {
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
                let Some(t) = target() else {
                    return Ok(Response::err(id, NOT_FOUND));
                };
                if t.key == me_key || t.role == Role::Founder {
                    return Ok(Response::err(id, BAD_REQUEST));
                }
                self.write(Kind::Remove, json!({"member": t.key}))?;
                let link = self.new_link("ask", false)?;
                Response::ok(id, json!({"name": t.name, "line": ticket::line(&link)}))
            }
            _ => Response::err(id, UNKNOWN_OP),
        })
    }

    fn write(&mut self, kind: Kind, body: Value) -> crate::Result<Entry> {
        write(self.daemon, self.team, self.store, kind, body, self.now)
    }

    /// Mints a link of `mode` off the cached roster (signed into the roster, recorded in
    /// `team_link`): its ticket.
    fn new_link(&mut self, mode: &str, once: bool) -> crate::Result<ticket::Ticket> {
        let (t, hashed) = ticket::mint(
            &self.daemon.identity,
            &self.team.roster,
            mode,
            once,
            self.now,
        );
        self.write(Kind::Link, ticket::link_body(&t, hashed.as_deref()))?;
        self.store.put_link(
            &t.ticket_id,
            &t.secret,
            hashed.as_deref(),
            mode,
            t.expires_at,
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
