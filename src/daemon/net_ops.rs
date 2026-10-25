//! The network ops, reached from `ipc::handle`'s fallback arm.

use super::ipc::{
    refused, Caller, Response, BAD_REQUEST, INTERNAL, NOT_FOUND, UNKNOWN_OP, UNSUPPORTED,
};
use super::{Daemon, TeamState};
use crate::agents::caller;
use crate::identity::Identity;
use crate::net::frames::Frame;
use crate::roster::{self, Entry, Kind, Roster};
use crate::store::Store;
use crate::{os, ticket};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Duration;

pub const NOT_ADMIN: &str = "not_admin";
pub const NOT_MEMBER: &str = "not_member";

/// `team.invite`'s `mode`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Invite {
    /// This member's live link, minted when it has none.
    #[default]
    Reusable,
    /// A new link in place of the live one, ending every earlier link of this computer.
    New,
    /// A link for one join.
    Once,
    /// A link whose joins need no Allow (founder and admins).
    Open,
    /// A once link for one person's computer, admitted with no Allow (an instance only).
    Person,
}

impl Invite {
    fn link_mode(self) -> &'static str {
        match self {
            Invite::Open => "open",
            Invite::Person => "person",
            _ => "ask",
        }
    }

    fn once(self) -> bool {
        matches!(self, Invite::Once | Invite::Person)
    }
}

/// Who may run a team op: a person at the socket (not an agent) when `person_only`, and a
/// member of the network when `member` (else even a computer the team removed).
struct Gate {
    person_only: bool,
    member: bool,
}

fn gate(op: &str) -> Gate {
    Gate {
        person_only: op == "team.remove",
        member: matches!(op, "team.remove" | "team.invite"),
    }
}

pub fn handle(id: u64, op: &str, body: &Value, daemon: &Daemon, caller: Caller) -> Response {
    // Retired: the cloud-member unlink; any member is removed with `team.remove`.
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
        // A person link is an instance's alone, and an instance mints no other link.
        if op == "team.invite" && (invite == Invite::Person) != (caller == Caller::Instance) {
            return Response::err(id, UNSUPPORTED);
        }
        let gate = gate(op);
        // Removal needs a person at the socket, before any authority check; an invite, a join
        // and an answer also take a resolved agent session (`Caller::actor`).
        if gate.person_only && !caller.is_person() {
            return refused(id, "not_a_person");
        }
        if op == "team.invite"
            && caller != Caller::Instance
            && caller.actor(&daemon.store.lock().unwrap()).is_none()
        {
            return refused(id, "not_a_person");
        }
        match op {
            "team.admit" => {
                let actor = caller.actor(&daemon.store.lock().unwrap());
                let Some(actor) = actor else {
                    return refused(id, "not_a_person");
                };
                let by = Some(actor).filter(|a| a != "local");
                return crate::admission::admit_op(id, body, daemon, by);
            }
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
        return match ops.run(op, body, invite, gate.member) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{op}: {e}");
                Response::err(id, INTERNAL)
            }
        };
    }
    match op {
        // `identity.get {}` -> who this computer (or instance, named for its workspace) is; its
        // `person_id` is its own key.
        "identity.get" => {
            let t = daemon.team.lock().unwrap();
            let me = &daemon.identity.key;
            let name = match &daemon.cloud {
                Some(c) => c.name.clone(),
                None => super::user_name(),
            };
            Response::ok(
                id,
                json!({
                    "member_key": me,
                    "member_key_short": daemon.identity.short,
                    "name": name,
                    "computer": t.config.computer,
                    "person_id": t.roster.person_of(me),
                    "solo": t.roster.is_solo(me),
                }),
            )
        }
        // `relay.get {}` -> `{urls, source}`: the relays in effect and where they came
        // from (`config`, `team`, `default` or `none`); the snapshot `status` reads too.
        "relay.get" => Response::ok(id, json!(*daemon.peers.relays.lock().unwrap())),
        "relay.set" => relay_set(id, body, daemon, caller),
        // `ui.claim {}` -> `{lease_s}`: the Lightbulb app shows the person's questions itself;
        // no OS dialog opens until it stops heartbeating.
        "ui.claim" if !caller.is_person() => refused(id, "not_a_person"),
        "ui.claim" => {
            daemon.dialogs.claim(super::ipc::now() as i64);
            Response::ok(id, json!({ "lease_s": crate::dialog::LEASE_S }))
        }
        // `snapshot {path}` -> `{}`: a consistent copy of `state.db` at `path` (absolute, outside
        // the state dir). On a computer a person only: it copies every message out.
        "snapshot" if !(caller.is_person() || caller == Caller::Instance) => {
            refused(id, "not_a_person")
        }
        "snapshot" => match body["path"].as_str().filter(|p| outside(&daemon.dir, p)) {
            Some(p) => match daemon
                .store
                .lock()
                .unwrap()
                .snapshot(std::path::Path::new(p))
            {
                Ok(()) => Response::ok(id, json!({})),
                Err(e) => {
                    eprintln!("snapshot: {e}");
                    Response::err(id, INTERNAL)
                }
            },
            None => Response::err(id, BAD_REQUEST),
        },
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
    let config = crate::config::Config::load(&daemon.dir)?;
    let off = super::canonical_off(&config);
    if daemon.team.lock().unwrap().set_config(config, off) {
        daemon.presence.notify_one();
    }
    Ok(())
}

/// `relay.set {url, local}` -> `{team}`: writes `config.toml: relay` (an empty or missing
/// `url` clears it); from a member, unless `local`, also the roster's
/// `meta {relay}`, so the whole team adopts it (`team: true`). In effect before the answer.
/// A person only: it moves where this computer's traffic goes.
fn relay_set(id: u64, body: &Value, daemon: &Daemon, caller: Caller) -> Response {
    if !caller.is_person() {
        return refused(id, "not_a_person");
    }
    let url = body["url"].as_str().unwrap_or_default().trim();
    if !url.is_empty() && crate::relay::parse(url).is_none() {
        return Response::err(id, BAD_REQUEST);
    }
    let mut team = daemon.team.lock().unwrap();
    let store = daemon.store.lock().unwrap();
    let me = &daemon.identity.key;
    let member = team.roster.members.contains_key(me);
    let elevated = member && body["local"] != true;
    match set_relay(daemon, &mut team, &store, url, elevated) {
        Ok(()) => {
            daemon.peers.relays_changed(&team);
            eprintln!("relay.set: {url:?} (team: {elevated})");
            Response::ok(id, json!({"team": elevated}))
        }
        Err(e) => {
            eprintln!("relay.set: {e}");
            Response::err(id, INTERNAL)
        }
    }
}

/// `config.toml: relay` = `url`, here and on disk; with `team`, the roster's `meta {relay}`.
fn set_relay(
    daemon: &Daemon,
    team: &mut TeamState,
    store: &Store,
    url: &str,
    meta: bool,
) -> crate::Result<()> {
    crate::config::Config::set(&daemon.dir, "relay", url)?;
    team.config.relay = Some(url.to_string());
    if meta {
        let now = super::ipc::now() as i64;
        write(daemon, team, store, Kind::Meta, json!({"relay": url}), now)?;
    }
    Ok(())
}

/// How long `team.leave` waits for a peer to take the `leave` entry.
const TELL_WAIT: Duration = Duration::from_secs(5);

/// `team.leave {}` -> `{name, told}`: writes this computer's `leave` entry and sends it to the
/// online members, waiting up to 5 s for one to take it (it then drops the connection, as to
/// any computer no longer a member); `told` is false when none did. Then it forgets the team
/// and starts this computer's solo network. `person`: a person asked (an embedded instance's caller
/// counts as one: the bridge acts for a workspace admin who confirmed in the app).
pub async fn leave(id: u64, daemon: &Daemon, person: bool) -> Response {
    if !person {
        return refused(id, "not_a_person");
    }
    let me = daemon.identity.key.clone();
    let left = {
        let mut team = daemon.team.lock().unwrap();
        let store = daemon.store.lock().unwrap();
        let r = &team.roster;
        let role = r.members.get(&me).map(|m| m.role);
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
        let now = super::ipc::now() as i64;
        let cloud = daemon.cloud.as_ref().map(|c| c.name.as_str());
        let at = now.max(root_at + 1);
        let g = solo_genesis(&daemon.identity, cloud, &team.config.computer, at);
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
        "role": "founder", "ticket_id": "", "team_name": team_name});
    Entry::signed(me, vec![], Kind::Admit, body, at)
}

/// This engine's solo network (`genesis`): a computer's, named for its user, or an instance's
/// (`cloud`: the workspace's name, member and team alike).
pub(crate) fn solo_genesis(me: &Identity, cloud: Option<&str>, computer: &str, at: i64) -> Entry {
    let user = super::user_name();
    let name = cloud.unwrap_or(&user);
    genesis(me, name, computer, name, at)
}

/// Whether `path` is absolute and names nothing in (or of) state dir `dir`: a snapshot never
/// lands on the live database.
pub(crate) fn outside(dir: &std::path::Path, path: &str) -> bool {
    let p = std::path::Path::new(path);
    let canon = |q: &std::path::Path| std::fs::canonicalize(q).ok();
    let (Some(dir), Some(parent)) = (canon(dir), p.parent().and_then(canon)) else {
        return false;
    };
    p.is_absolute()
        && p.file_name().is_some()
        && !parent.starts_with(&dir)
        && !canon(p).is_some_and(|c| c.starts_with(&dir))
}

/// Rewrites Plan A's `team` row from `new` when its id, name or founder differs from `old`'s.
pub(crate) fn sync_team_row(store: &Store, old: &Roster, new: &Roster) -> crate::Result<()> {
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
/// cached roster, rewrites Plan A's `team` row if it changed, and tells `net` (which closes
/// the connections of members gone, dials new ones and pushes the change).
pub fn install(
    daemon: &Daemon,
    team: &mut TeamState,
    store: &Store,
    new: Roster,
) -> crate::Result<()> {
    sync_team_row(store, &team.roster, &new)?;
    let me = daemon.identity.key.as_str();
    // A removal of this computer, signed by an admin, that this install brings.
    let removed = removal_notice(&team.roster, &new, me);
    if !new.is_solo(me) {
        let _ = std::fs::remove_file(daemon.dir.join(REMOVED));
    }
    // A computer gone from the team is nobody's any more (`persons`).
    store.prune_persons(|k| new.members.contains_key(k))?;
    let relay_changed = team.roster.relay != new.relay;
    // A removal or leave applied in this team (not a team left or joined, nor a member a fork's
    // resolution dropped): every subscriber hears of each member it took.
    if team.roster.team_id == new.team_id {
        let took = |k: &str| {
            new.log().iter().any(|(h, e)| {
                matches!(e.kind, Kind::Remove | Kind::Leave)
                    && !team.roster.knows(h)
                    && e.body["member"] == k
            })
        };
        for k in team
            .roster
            .members
            .keys()
            .filter(|k| !new.members.contains_key(*k) && took(k))
        {
            let _ = daemon
                .events
                .send(json!({"event": "member_removed", "member": k}));
        }
    }
    team.roster = new;
    daemon.peers.prune_known(&daemon.dir, &team.roster);
    daemon.peers.changed.notify_one();
    if relay_changed {
        // Logged once per value: an install changes it once.
        if let Some(v) = team.roster.relay.as_deref() {
            if !v.trim().is_empty() && crate::relay::parse(v).is_none() {
                eprintln!("relay: ignored the team's {v:?} (not an https relay URL)");
            }
        }
        daemon.peers.relays_changed(team);
    }
    daemon.roster_changed.notify_waiters();
    if let Some((notice, root_at)) = removed {
        return removed_here(daemon, team, store, notice, root_at);
    }
    Ok(())
}

/// The file that keeps `team` saying this computer was removed, until it is in a team again.
pub const REMOVED: &str = "removed.json";

#[derive(Serialize, Deserialize)]
pub struct Removed {
    pub team: String,
    pub by: String,
    pub at: i64,
}

/// The kept `Removed` notice, while this computer is on its own network after a removal.
pub fn removed_notice(daemon: &Daemon) -> Option<Removed> {
    serde_json::from_slice(&std::fs::read(daemon.dir.join(REMOVED)).ok()?).ok()
}

/// The notice for a removal of `me` that going from `old` to `new` brings (a `remove` of it
/// signed in this team, not known before), with `old`'s root time.
fn removal_notice(old: &Roster, new: &Roster, me: &str) -> Option<(Removed, i64)> {
    if old.team_id != new.team_id || !old.members.contains_key(me) || new.members.contains_key(me) {
        return None;
    }
    let (_, e) = new
        .log()
        .iter()
        .find(|(h, e)| e.kind == Kind::Remove && !old.knows(h) && e.body["member"] == me)?;
    let by = old.members.get(&e.signer).map(|m| m.name.clone());
    let notice = Removed {
        team: old.team_name.clone(),
        by: by.unwrap_or_else(|| crate::net::short(&e.signer).to_string()),
        at: e.at,
    };
    Some((notice, old.created_at))
}

/// This computer was removed (`install` saw the entry): mail still queued for the team is
/// refused (`not_member`) at once, the notice is kept for `team`, and the computer is on its
/// own network again, as after `leave`.
fn removed_here(
    daemon: &Daemon,
    team: &mut TeamState,
    store: &Store,
    notice: Removed,
    root_at: i64,
) -> crate::Result<()> {
    let now = super::ipc::now();
    crate::outbox::refuse_all(store, now, "not_member")?;
    eprintln!(
        "net: removed from \"{}\" by {}; on our own network again",
        notice.team, notice.by
    );
    store.clear_roster()?;
    let cloud = daemon.cloud.as_ref().map(|c| c.name.as_str());
    let at = (now as i64).max(root_at + 1);
    let g = solo_genesis(&daemon.identity, cloud, &team.config.computer, at);
    roster::accept(store, &g, now as i64)?;
    install(daemon, team, store, roster::replay(&[g]))?;
    std::fs::write(daemon.dir.join(REMOVED), serde_json::to_vec(&notice)?)?;
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
        member: bool,
    ) -> crate::Result<Response> {
        let id = self.id;
        let identity = &self.daemon.identity;
        let me_key = identity.key.clone();
        let r = &self.team.roster;
        // Membership first: a computer the team no longer holds is `not_member`.
        if member && !r.members.contains_key(&me_key) {
            return Ok(Response::err(id, NOT_MEMBER));
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
                        "heads": r.heads, "removed": removed_notice(self.daemon)}),
                )
            }
            // `team.invite {mode: "person", for, name}` -> `{line, expires_at}`: a once link
            // for person `for`'s computer (24 h), its line with `name` filled in.
            "team.invite" if invite == Invite::Person => {
                // Joinable as is (`admission::fits`) and safe inside the line's quotes.
                let quotable = |n: &&str| crate::admission::fits(n) && !n.contains('\'');
                let person = body["for"].as_str().filter(|p| !p.is_empty());
                let (Some(person), Some(name)) = (person, body["name"].as_str().filter(quotable))
                else {
                    return Ok(Response::err(id, BAD_REQUEST));
                };
                let mut t = self.new_link(invite.link_mode(), true, false)?;
                // A workspace member's line carries its doorbell: the person may paste it
                // after the workspace parked, and the joiner has no roster to ring from.
                t.wake = self.team.roster.wakes.get(&me_key).cloned();
                let t = ticket::for_person(&self.daemon.identity, t, person);
                let line = ticket::person_line(&ticket::encode(&t), name);
                Response::ok(id, json!({"line": line, "expires_at": t.expires_at}))
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
                let replaced = invite == Invite::New
                    && r.links
                        .values()
                        .any(|l| l.signer == me_key && l.expires_at > self.now);
                let t = match live {
                    Some(l) if invite != Invite::New => {
                        let hint = self.daemon.peers.hints();
                        ticket::sign(identity, r, l.ticket_id, l.secret, mode, l.expires_at, hint)
                    }
                    // `--new` ends every earlier link of this computer, `open` and once ones too.
                    _ => self.new_link(mode, invite.once(), invite == Invite::New)?,
                };
                let s = ticket::encode(&t);
                Response::ok(
                    id,
                    json!({"line": ticket::join_line(&s), "app_link": ticket::app_link(&s),
                        "link": ticket::web_link(&s),
                        "ticket": s, "expires_at": t.expires_at, "replaced": replaced}),
                )
            }
            // `team.remove {member}` -> `{name, line}`: the removal ends every live link, so a
            // new one follows it.
            "team.remove" => {
                let Some(t) = target() else {
                    return Ok(Response::err(id, NOT_FOUND));
                };
                if t.key == me_key {
                    return Ok(Response::err(id, BAD_REQUEST));
                }
                self.write(Kind::Remove, json!({"member": t.key}))?;
                let link = self.new_link("ask", false, false)?;
                Response::ok(id, json!({"name": t.name, "line": ticket::line(&link)}))
            }
            _ => Response::err(id, UNKNOWN_OP),
        })
    }

    fn write(&mut self, kind: Kind, body: Value) -> crate::Result<Entry> {
        write(self.daemon, self.team, self.store, kind, body, self.now)
    }

    /// Mints a link of `mode` off the cached roster (signed into the roster, recorded in
    /// `team_link`; with `ends_all`, ending every earlier link of ours): its ticket.
    fn new_link(
        &mut self,
        mode: &str,
        once: bool,
        ends_all: bool,
    ) -> crate::Result<ticket::Ticket> {
        let (t, hashed) = ticket::mint(
            &self.daemon.identity,
            &self.team.roster,
            mode,
            once,
            self.now,
            self.daemon.peers.hints(),
        );
        let mut body = ticket::link_body(&t, hashed.as_deref());
        if ends_all {
            body["ends_all"] = true.into();
        }
        self.write(Kind::Link, body)?;
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

/// A fresh once `ask` link for an invite code to serve (`code::offer`): its ticket.
pub(crate) fn code_link(daemon: &Daemon) -> crate::Result<ticket::Ticket> {
    let mut team = daemon.team.lock().unwrap();
    let store = daemon.store.lock().unwrap();
    let mut ops = TeamOps {
        id: 0,
        daemon,
        team: &mut team,
        store: &store,
        now: super::ipc::now() as i64,
    };
    ops.new_link("ask", true, false)
}

/// The published sessions of member `short` that `list` declares (`cloud.sessions.set`, the
/// sessions file at start): each `{name, agent, title?}`, names valid and distinct, at most
/// `presence::MAX`. `None` if any is not.
pub(crate) fn remotes(short: &str, list: &[Value]) -> Option<Vec<crate::net::frames::Remote>> {
    if list.len() > crate::presence::MAX {
        return None;
    }
    let mut names = HashSet::new();
    let mut out = vec![];
    for s in list {
        let (name, agent) = (s["name"].as_str()?, s["agent"].as_str()?);
        if !crate::sessions::valid_name(name) || !names.insert(name) {
            return None;
        }
        let r = crate::net::frames::Remote {
            session_id: format!("{short}/{name}"),
            agent: agent.into(),
            repo: None,
            branch: None,
            folder: String::new(),
            status: "idle".into(),
            doing: None,
            tier: crate::sessions::Tier::Pull.as_str().into(),
            title: s["title"].as_str().map(String::from),
        }
        .clamp();
        if !r.valid(short) {
            return None;
        }
        out.push(r);
    }
    Some(out)
}

/// A present `members` value (person ids): `null` is no gate, a list of strings is the gate;
/// anything else (a non-list, or a list with a non-string) is `Err`.
pub(crate) fn members_of(v: &Value) -> Result<Option<HashSet<String>>, ()> {
    match v {
        Value::Null => Ok(None),
        Value::Array(l) => l
            .iter()
            .map(|p| p.as_str().map(String::from).ok_or(()))
            .collect::<Result<_, _>>()
            .map(Some),
        _ => Err(()),
    }
}

/// `cloud.sessions.set {sessions: [{name, agent, title?}], members?}` -> `{}` (an embedded instance only;
/// `unsupported` on a computer): the workspace's agents, published as `<short>/<name>` with
/// `agent` and `title` (at most `presence::MAX`, each name a `valid_name`, once). Answers once
/// the presence sender (`presence::run`) has handed on the presence carrying them, so a `send`
/// from a name just declared is never refused as unadvertised by a peer the envelope reaches
/// first; `internal` if that takes over 5 s (the list stands, and goes out on the next send).
/// `members` replaces the sessions' member gate (`mail::receive`): a list sets it, `null`
/// removes it, absent keeps it; anything else is `bad_request`, before anything changes.
pub async fn cloud_sessions_set(id: u64, body: &Value, daemon: &Daemon) -> Response {
    let Some(cloud) = &daemon.cloud else {
        return Response::err(id, UNSUPPORTED);
    };
    let Some(out) = body["sessions"]
        .as_array()
        .and_then(|l| remotes(&daemon.identity.short, l))
    else {
        return Response::err(id, BAD_REQUEST);
    };
    let members = match body.get("members") {
        None => cloud.declared().members.map(|m| (*m).clone()),
        Some(v) => match members_of(v) {
            Ok(m) => m,
            Err(()) => return Response::err(id, BAD_REQUEST),
        },
    };
    let mut sent = cloud.sent.subscribe();
    let version = cloud.set(out, members);
    daemon.presence.notify_one();
    let handed = sent.wait_for(|v| *v >= version);
    let handed = matches!(
        tokio::time::timeout(Duration::from_secs(5), handed).await,
        Ok(Ok(_))
    );
    if handed {
        Response::ok(id, json!({}))
    } else {
        Response::err(id, INTERNAL)
    }
}

/// The session of the agent above `peer_pid`, as `<member>/<handle>`.
/// `agent` before it registers one. `None` when no agent is above `peer_pid`.
pub fn agent_session(store: &Store, peer_pid: u32) -> Option<String> {
    let (agent, _) = caller::ancestors_from(peer_pid)
        .filter_map(Result::ok)
        .find(|(_, is_agent)| *is_agent)?;
    let rows = store.sessions().ok()?;
    let found = rows.into_iter().find(|s| {
        s.pid == Some(agent.pid.into()) && s.birth_micros == i64::try_from(agent.birth_micros).ok()
    });
    Some(found.map_or("agent".into(), |s| {
        s.handle.map_or(s.key.clone(), |h| {
            format!("{}/{h}", s.key.split('/').next().unwrap_or_default())
        })
    }))
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

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
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
    fn an_unresolved_caller_is_nobody() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        assert_eq!(super::super::ipc::Caller::Socket(None).actor(&store), None);
        assert_eq!(super::super::ipc::Caller::Instance.actor(&store), None);
        assert_eq!(
            super::super::ipc::Caller::Socket(Some(1))
                .actor(&store)
                .as_deref(),
            Some("local")
        );
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

    /// A same-uid process that made itself non-dumpable hides its `exe`: the walk through it
    /// fails closed (not a person), never falls back to the name it chose.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_hidden_same_uid_process_is_not_a_person() {
        let (mut hidden, pid) = child_of(
            &std::env::current_exe().unwrap(),
            &[
                "--exact",
                "daemon::net_ops::tests::nondumpable_sleep",
                "--ignored",
                "--nocapture",
            ],
        );
        assert!(os::process(pid).is_err());
        assert!(!no_agent(caller::ancestors_pinned(pid, None)));
        kill(pid);
        hidden.wait().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "a helper the test above runs: non-dumpable, prints its pid, sleeps"]
    fn nondumpable_sleep() {
        // SAFETY: a plain prctl on this process.
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) }, 0);
        println!("{}", std::process::id());
        std::thread::sleep(std::time::Duration::from_secs(30));
    }

    #[test]
    #[ignore = "a helper the test above runs as an agent"]
    fn sleep_under_me() {
        let mut sleep = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        println!("{}", sleep.id());
        sleep.wait().unwrap();
    }
}
