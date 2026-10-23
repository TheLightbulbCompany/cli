//! Presence: this computer's sessions as its teammates see them, sent after each `hello` and
//! on change; the latest from each member, kept in `peers_sessions`; and the `directory` op
//! that merges both.

use crate::daemon::{ipc, Daemon};
use crate::net::frames::{Frame, Remote};
use crate::roster::{Member, MemberKind, Role, Roster};
use crate::sessions::{Session, Status};
use crate::store::Store;
use rusqlite::params;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// At most this many sessions, `known` and `kept` addresses each per presence.
pub const MAX: usize = 100;
/// How long an ended session's or a name's address stays kept, and how long an offline
/// member's presence is listed.
pub const DAY: u64 = 86_400;
/// Changes this close together go out as one presence.
const COALESCE: Duration = Duration::from_millis(250);
/// What is read from transcripts (`doing`, `title`) and process tables has no event: the
/// presence is rebuilt this often and sent when it changed.
const TICK: Duration = Duration::from_secs(5);

/// This computer's person: its roster member's `person_id`, else its own key.
pub fn my_person(d: &Daemon, r: &Roster) -> String {
    r.members
        .get(&d.identity.key)
        .map_or_else(|| d.identity.key.clone(), |m| m.person_id.clone())
}

/// Keeps `key` (an ended session in `cwd`, or a name, `cwd` empty) for a day from now, and
/// wakes the sender.
pub fn keep(d: &Daemon, key: &str, cwd: &str) {
    let at = (ipc::now(), cwd.to_string());
    d.kept.lock().unwrap().insert(key.into(), at);
    d.presence.notify_one();
}

/// `s` as published: `folder` is `cwd`'s last part, or all of it with `share_paths`.
fn remote(s: &Session, share_paths: bool) -> Remote {
    let (repo, branch) = crate::mcp::git(&s.cwd);
    let folder = if share_paths {
        s.cwd.clone()
    } else {
        Path::new(&s.cwd)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    Remote {
        session_id: s.key.0.clone(),
        agent: s.provider.clone(),
        repo,
        branch,
        folder,
        status: if s.status == Status::Live {
            "busy"
        } else {
            "idle"
        }
        .into(),
        doing: s.doing.clone(),
        tier: serde_json::to_value(s.tier)
            .ok()
            .and_then(|t| t.as_str().map(String::from))
            .unwrap_or_default(),
        title: s.title.clone(),
    }
}

/// This computer's presence, `seq` 0: its live sessions (newest first), the non-live
/// addresses still holding mail (`known`, newest mail first) and the kept ones (`kept`,
/// newest first); nothing under a `folders_off` folder.
fn build(d: &Daemon) -> Frame {
    let now = ipc::now();
    let (share, off) = {
        let t = d.team.lock().unwrap();
        (t.config.share_paths, t.config.folders_off.clone())
    };
    let is_off = |cwd: &str| off.iter().any(|o| Path::new(cwd).starts_with(o));
    let mut live = ipc::live_sessions(d);
    live.retain(|s| !is_off(&s.cwd));
    live.sort_by_key(|s| std::cmp::Reverse(s.birth_micros));
    live.truncate(MAX);
    let live_keys: HashSet<&str> = live.iter().map(|s| s.key.0.as_str()).collect();
    let mine = format!("{}/", d.identity.short);
    let store = d.store.lock().unwrap();
    let rows = store.sessions().unwrap_or_default();
    let off_key = |k: &str| rows.iter().any(|r| r.key == k && is_off(&r.cwd));
    let known: Vec<String> = waiting_keys(&store, now)
        .into_iter()
        .filter(|k| k.starts_with(&mine) && !live_keys.contains(k.as_str()) && !off_key(k))
        .take(MAX)
        .collect();
    // Ended (`keep`) and gone (a row whose process died) addresses seen within the day.
    let mut kept: Vec<(u64, String)> = {
        let mut map = d.kept.lock().unwrap();
        map.retain(|_, (at, _)| *at + DAY >= now);
        map.iter()
            .filter(|(_, (_, cwd))| !is_off(cwd))
            .map(|(k, (at, _))| (*at, k.clone()))
            .collect()
    };
    for r in rows.iter().filter(|r| !is_off(&r.cwd)) {
        kept.push((u64::try_from(r.seen_at).unwrap_or(0), r.key.clone()));
    }
    kept.sort_by(|a, b| b.cmp(a));
    let mut seen = HashSet::new();
    let kept: Vec<String> = kept
        .into_iter()
        .filter(|(at, k)| {
            at + DAY >= now
                && k.starts_with(&mine)
                && !live_keys.contains(k.as_str())
                && seen.insert(k.clone())
        })
        .map(|(_, k)| k)
        .take(MAX)
        .collect();
    Frame::Presence {
        member: d.identity.key.clone(),
        seq: 0,
        sessions: live.iter().map(|s| remote(s, share)).collect(),
        known,
        kept,
    }
}

/// The addresses with waiting mail, newest mail first.
fn waiting_keys(store: &Store, now: u64) -> Vec<String> {
    let q =
        "SELECT to_key FROM mailbox WHERE expires_at >= ?1 GROUP BY to_key ORDER BY MAX(id) DESC";
    let Ok(mut stmt) = store.conn().prepare(q) else {
        return vec![];
    };
    stmt.query_map([now as i64], |r| r.get(0))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

/// `f` stamped with the time.
fn stamped(mut f: Frame) -> Frame {
    if let Frame::Presence { seq, .. } = &mut f {
        *seq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
    }
    f
}

/// The presence to send one peer now (after its `hello`).
pub fn current(d: &Daemon) -> Frame {
    stamped(build(d))
}

/// The sender: on a local change (`Daemon::presence`, coalesced over 250 ms) or every 5 s,
/// sends this computer's presence to every online member when it differs from the last sent.
pub async fn run(d: Arc<Daemon>) {
    let mut last = None;
    loop {
        tokio::select! {
            _ = d.presence.notified() => tokio::time::sleep(COALESCE).await,
            _ = tokio::time::sleep(TICK) => {}
        }
        let f = build(&d);
        if last.as_ref() == Some(&f) {
            continue;
        }
        let out = stamped(f.clone());
        for m in d.peers.online() {
            let _ = d.peers.send(&m, out.clone());
        }
        last = Some(f);
    }
}

/// Stores `member`'s presence `f` unless the one stored is as new (`seq`).
pub fn put(store: &Store, member: &str, f: &Frame, now: u64) -> rusqlite::Result<()> {
    let Frame::Presence { seq, .. } = f else {
        return Ok(());
    };
    let text = serde_json::to_string(f).unwrap_or_default();
    store.conn().execute(
        "INSERT INTO peers_sessions VALUES (?1, ?2, ?3, ?4) ON CONFLICT(member) DO UPDATE SET \
         seq = excluded.seq, presence = excluded.presence, seen_at = excluded.seen_at \
         WHERE excluded.seq > peers_sessions.seq",
        params![member, *seq as i64, text, now as i64],
    )?;
    Ok(())
}

/// `member` was last seen now (its connection closed): its presence is listed a day more.
pub fn seen(d: &Daemon, member: &str) {
    let store = d.store.lock().unwrap();
    let _ = store.conn().execute(
        "UPDATE peers_sessions SET seen_at = ?2 WHERE member = ?1",
        params![member, ipc::now() as i64],
    );
}

/// The stored presences of current members, `(member, sessions, kept)`; a member offline
/// and unseen for a day is dropped.
fn stored(d: &Daemon) -> Vec<(Member, Vec<Remote>, Vec<String>)> {
    let online: HashSet<String> = d.peers.online().into_iter().collect();
    let now = ipc::now() as i64;
    let team = d.team.lock().unwrap();
    let store = d.store.lock().unwrap();
    let rows: Vec<(String, String, i64)> = store
        .conn()
        .prepare("SELECT member, presence, seen_at FROM peers_sessions")
        .and_then(|mut s| {
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect()
        })
        .unwrap_or_default();
    let mut out = vec![];
    for (member, text, seen_at) in rows {
        if seen_at + (DAY as i64) < now && !online.contains(&member) {
            let _ = store
                .conn()
                .execute("DELETE FROM peers_sessions WHERE member = ?1", [&member]);
            continue;
        }
        let Some(m) = team.roster.members.get(&member) else {
            continue;
        };
        if let Ok(Frame::Presence { sessions, kept, .. }) = serde_json::from_str(&text) {
            out.push((m.clone(), sessions, kept));
        }
    }
    out
}

/// Whether another computer of this person keeps `rest` (the part of an address after the
/// member prefix), by its latest presence here. Best effort: only what is stored.
pub fn kept_elsewhere(d: &Daemon, rest: &str) -> bool {
    let me = {
        let t = d.team.lock().unwrap();
        my_person(d, &t.roster)
    };
    stored(d).iter().any(|(m, _, kept)| {
        m.key != d.identity.key
            && m.person_id == me
            && kept
                .iter()
                .any(|k| k.split_once('/').is_some_and(|(_, r)| r == rest))
    })
}

/// A directory record (architecture §9); `cwd` is empty for another computer's session.
fn record(m: &Member, me: &str, s: &Remote, cwd: &str) -> Value {
    json!({
        "session_id": s.session_id,
        "person": m.name,
        "person_verified": m.proof.is_some(),
        "computer": m.computer,
        "agent": s.agent,
        "cwd": cwd,
        "repo": s.repo,
        "branch": s.branch,
        "status": s.status,
        "doing": s.doing.clone().unwrap_or_default(),
        "tier": s.tier,
        "can_send": true,
        "needs_owner_ok": m.person_id != me,
    })
}

/// `directory {query?}` -> `{sessions}`: this computer's sessions, then every member's latest
/// presence; `query` matches session id, person, computer, agent, repo or folder, any case.
pub fn directory(d: &Daemon, query: Option<&str>) -> Value {
    let local = ipc::live_sessions(d);
    let remote_ones = stored(d);
    let (me, own) = {
        let t = d.team.lock().unwrap();
        let own = t.roster.members.get(&d.identity.key).cloned();
        let own = own.unwrap_or_else(|| Member {
            key: d.identity.key.clone(),
            short: d.identity.short.clone(),
            name: crate::daemon::user_name(),
            computer: t.config.computer.clone(),
            person_id: d.identity.key.clone(),
            role: Role::Member,
            kind: MemberKind::Computer,
            admitted_at: 0,
            proof: None,
        });
        (my_person(d, &t.roster), own)
    };
    let q = query.map(str::to_lowercase).filter(|q| !q.is_empty());
    let hit = |m: &Member, s: &Remote| {
        q.as_ref().is_none_or(|q| {
            [&s.session_id, &m.name, &m.computer, &s.agent, &s.folder]
                .into_iter()
                .chain(s.repo.as_ref())
                .any(|f| f.to_lowercase().contains(q))
        })
    };
    let mut out = vec![];
    for s in &local {
        let r = remote(s, false);
        if hit(&own, &r) {
            out.push(record(&own, &me, &r, &s.cwd));
        }
    }
    for (m, sessions, _) in &remote_ones {
        for r in sessions.iter().filter(|r| hit(m, r)) {
            out.push(record(m, &me, r, ""));
        }
    }
    json!({ "sessions": out })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_older_presence_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        let p = |seq, member: &str| Frame::Presence {
            member: member.into(),
            seq,
            sessions: vec![],
            known: vec![format!("x/{seq}")],
            kept: vec![],
        };
        for seq in [5, 3, 5, 7] {
            put(&store, "m", &p(seq, "m"), 1).unwrap();
        }
        let text: String = store
            .conn()
            .query_row("SELECT presence FROM peers_sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(serde_json::from_str::<Frame>(&text).unwrap(), p(7, "m"));
    }
}
