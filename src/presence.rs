//! Presence: this computer's sessions as its teammates see them, sent after each `hello` and
//! on change; the latest from each member (in memory, `peers_sessions` its copy across
//! restarts); and the `directory` op that merges both.

use crate::daemon::{ipc, Daemon};
use crate::mail;
use crate::net::frames::{Frame, Remote, STATUSES};
use crate::roster::{Member, MemberKind, Role, Roster};
use crate::sessions::{split_addr, Session, Status};
use crate::store::Store;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
/// An unchanged presence is sent again this often.
const RESEND: Duration = Duration::from_secs(60);
/// The least time between two builds.
const GAP: Duration = Duration::from_secs(1);

/// A member's latest presence, as held.
#[derive(Clone, Debug, PartialEq)]
pub struct Latest {
    pub seq: u64,
    pub seen_at: u64,
    pub sessions: Vec<Remote>,
    pub kept: Vec<String>,
}

/// The stored presences, by member (read at daemon start).
pub fn load(store: &Store) -> HashMap<String, Latest> {
    let rows = store.peers_presence().unwrap_or_default();
    rows.into_iter()
        .filter_map(
            |(member, text, seen_at)| match serde_json::from_str(&text).ok()? {
                Frame::Presence {
                    seq,
                    sessions,
                    kept,
                    ..
                } => Some((
                    member,
                    Latest {
                        seq,
                        seen_at: u64::try_from(seen_at).unwrap_or(0),
                        sessions,
                        kept,
                    },
                )),
                _ => None,
            },
        )
        .collect()
}

/// This computer's person (`Roster::person_of` its key).
pub fn my_person(d: &Daemon, r: &Roster) -> String {
    r.person_of(&d.identity.key).to_string()
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
        status: STATUSES[usize::from(s.status != Status::Live)].into(),
        doing: s.doing.clone(),
        tier: s.tier.as_str().into(),
        title: s.title.clone(),
    }
    .clamp()
}

/// Whether `cwd` is under one of `off` (`folders_off`, each `canonical`).
pub(crate) fn hidden(off: &[PathBuf], cwd: &str) -> bool {
    !off.is_empty() && {
        let cwd = canonical(Path::new(cwd));
        off.iter().any(|o| cwd.starts_with(o))
    }
}

/// `p` resolved (symlinks, `..`); a path that does not exist (a deleted folder) is its nearest
/// existing parent resolved, then the rest as given. Lowercased on macOS, whose volumes ignore case.
pub(crate) fn canonical(p: &Path) -> PathBuf {
    let p = p
        .ancestors()
        .find_map(|a| {
            let rest = p.strip_prefix(a).ok()?;
            Some(std::fs::canonicalize(a).ok()?.join(rest))
        })
        .unwrap_or_else(|| p.to_path_buf());
    if cfg!(target_os = "macos") {
        PathBuf::from(p.to_string_lossy().to_lowercase())
    } else {
        p
    }
}

/// This computer's presence: its live sessions (newest first), the non-live addresses still
/// holding mail (`known`, newest mail first) and the kept ones (`kept`, newest first); nothing
/// whose folder is under `folders_off`, nor an address whose folder it does not know.
fn build(d: &Daemon, seq: u64) -> Frame {
    let now = ipc::now();
    let (share, off) = {
        let t = d.team.lock().unwrap();
        (t.config.share_paths, t.config.folders_off.clone())
    };
    let off: Vec<PathBuf> = off.iter().map(|o| canonical(o)).collect();
    let publishable = |cwd: &str| !hidden(&off, cwd);
    let mut live = ipc::live_sessions(d);
    let (rows, ended, waiting) = {
        let store = d.store.lock().unwrap();
        let since = now.saturating_sub(DAY) as i64;
        (
            store.sessions().unwrap_or_default(),
            store.ended_since(since).unwrap_or_default(),
            mail::waiting_keys(&store, now).unwrap_or_default(),
        )
    };
    // Every address with a row here, `(key, last seen or ended, folder)`, if publishable.
    let mine = format!("{}/", d.identity.short);
    let rows = rows.into_iter().map(|r| (r.key, r.seen_at, r.cwd));
    let mut candidates: Vec<(String, i64, String)> = rows
        .chain(ended)
        .filter(|(k, _, cwd)| k.starts_with(&mine) && publishable(cwd))
        .collect();
    candidates.sort_by_key(|(_, at, _)| std::cmp::Reverse(*at));
    live.retain(|s| publishable(&s.cwd));
    live.sort_by_key(|s| std::cmp::Reverse(s.birth_micros));
    live.truncate(MAX);
    let live_keys: HashSet<&str> = live.iter().map(|s| s.key.0.as_str()).collect();
    let others = || {
        candidates
            .iter()
            .filter(|(k, ..)| !live_keys.contains(k.as_str()))
    };
    let known = waiting
        .into_iter()
        .filter(|k| others().any(|(c, ..)| c == k))
        .take(MAX)
        .collect();
    let kept = others()
        .filter(|(_, at, _)| *at + DAY as i64 >= now as i64)
        .map(|(k, ..)| k.clone())
        .take(MAX)
        .collect();
    Frame::Presence {
        member: d.identity.key.clone(),
        seq,
        sessions: live.iter().map(|s| remote(s, share)).collect(),
        known,
        kept,
    }
}

/// Whether `a` and `b` say the same, whatever their `seq`.
fn same(a: &Frame, b: &Frame) -> bool {
    match (a, b) {
        (
            Frame::Presence {
                sessions,
                known,
                kept,
                ..
            },
            Frame::Presence {
                sessions: s,
                known: k,
                kept: p,
                ..
            },
        ) => sessions == s && known == k && kept == p,
        _ => false,
    }
}

/// The presence to send a peer after its `hello`: the last built, stamped now; built here
/// only when there is none yet.
pub fn for_hello(d: &Daemon) -> Frame {
    let cached = d.built.lock().unwrap().clone();
    let mut f = cached.unwrap_or_else(|| {
        let f = build(d, 0);
        *d.built.lock().unwrap() = Some(f.clone());
        f
    });
    stamp(&mut f);
    f
}

/// The last `seq` sent: each send's is later, even within one millisecond.
static LAST_SEQ: AtomicU64 = AtomicU64::new(0);

/// `f`'s `seq` set to now, or just past the last sent.
fn stamp(f: &mut Frame) {
    if let Frame::Presence { seq, .. } = f {
        let now = ipc::now_ms();
        let next = |last: u64| now.max(last + 1);
        // A compare-exchange loop: `fetch_update` is deprecated on current stable (1.99) and its
        // replacement `try_update` is not on the oldest toolchain this builds with (1.96).
        let mut last = LAST_SEQ.load(Ordering::SeqCst);
        while let Err(l) =
            LAST_SEQ.compare_exchange_weak(last, next(last), Ordering::SeqCst, Ordering::SeqCst)
        {
            last = l;
        }
        *seq = next(last);
    }
}

/// The sender: on a local change (`Daemon::presence`, coalesced over 250 ms) or every 5 s,
/// at most once a second and only while a member is online, builds this computer's presence
/// and sends it to every online member when it changed, and every 60 s regardless.
pub async fn run(d: Arc<Daemon>) {
    let (mut built_at, mut sent_at): (Option<Instant>, Option<Instant>) = (None, None);
    loop {
        tokio::select! {
            _ = d.presence.notified() => tokio::time::sleep(COALESCE).await,
            _ = tokio::time::sleep(TICK) => {}
        }
        if d.peers.online().is_empty() {
            continue;
        }
        if let Some(at) = built_at {
            tokio::time::sleep(GAP.saturating_sub(at.elapsed())).await;
        }
        built_at = Some(Instant::now());
        let d2 = d.clone();
        let Ok(mut f) = tokio::task::spawn_blocking(move || build(&d2, 0)).await else {
            continue;
        };
        let unchanged = d
            .built
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|b| same(b, &f));
        *d.built.lock().unwrap() = Some(f.clone());
        if unchanged && sent_at.is_some_and(|at| at.elapsed() < RESEND) {
            continue;
        }
        // Stamped at send, after the store: a later send (here or a hello's) carries a later seq.
        stamp(&mut f);
        for m in d.peers.online() {
            let _ = d.peers.send(&m, f.clone());
        }
        sent_at = Some(Instant::now());
    }
}

/// Takes `l` as `member`'s latest unless the one held is as new (`seq`), or always with
/// `first` (the first on a connection, so a peer whose clock stepped back is not frozen).
fn accept(held: &mut HashMap<String, Latest>, member: &str, l: Latest, first: bool) -> bool {
    if !first && held.get(member).is_some_and(|o| l.seq <= o.seq) {
        return false;
    }
    held.insert(member.into(), l);
    true
}

/// Holds and stores `member`'s presence `f` by the `seq` rule (`accept`).
pub fn put(d: &Daemon, member: &str, f: &Frame, now: u64, first: bool) {
    let Frame::Presence {
        seq,
        sessions,
        kept,
        ..
    } = f
    else {
        return;
    };
    let l = Latest {
        seq: *seq,
        seen_at: now,
        sessions: sessions.clone(),
        kept: kept.clone(),
    };
    if accept(&mut d.latest.lock().unwrap(), member, l, first) {
        let text = serde_json::to_string(f).unwrap_or_default();
        let stored = d
            .store
            .lock()
            .unwrap()
            .put_presence(member, *seq, &text, now as i64);
        if let Err(e) = stored {
            eprintln!("presence: storing {}: {e}", crate::net::short(member));
        }
    }
}

/// `member` was last seen now (its connection closed): its presence is listed a day more.
pub fn seen(d: &Daemon, member: &str) {
    let now = ipc::now();
    if let Some(l) = d.latest.lock().unwrap().get_mut(member) {
        l.seen_at = now;
    }
    let _ = d.store.lock().unwrap().touch_presence(member, now as i64);
}

/// The latest presences of current members; one offline and unseen for a day is dropped.
fn stored(d: &Daemon) -> Vec<(Member, Latest)> {
    let online: HashSet<String> = d.peers.online().into_iter().collect();
    let now = ipc::now();
    let mut stale = vec![];
    let held: Vec<(String, Latest)> = {
        let mut held = d.latest.lock().unwrap();
        held.retain(|m, l| {
            let keep = l.seen_at + DAY >= now || online.contains(m);
            if !keep {
                stale.push(m.clone());
            }
            keep
        });
        held.iter().map(|(m, l)| (m.clone(), l.clone())).collect()
    };
    if !stale.is_empty() {
        let store = d.store.lock().unwrap();
        for m in &stale {
            let _ = store.drop_presence(m);
        }
    }
    let team = d.team.lock().unwrap();
    held.into_iter()
        .filter_map(|(m, l)| Some((team.roster.members.get(&m)?.clone(), l)))
        .collect()
}

/// Whether another computer of this person keeps `rest` (the part of an address after the
/// member prefix), by its latest presence here. Best effort: only what is held.
fn kept_elsewhere(d: &Daemon, rest: &str) -> bool {
    let me = {
        let t = d.team.lock().unwrap();
        my_person(d, &t.roster)
    };
    stored(d).iter().any(|(m, l)| {
        m.key != d.identity.key
            && m.person_id == me
            && l.kept
                .iter()
                .any(|k| split_addr(k).is_some_and(|(_, r)| r == rest))
    })
}

/// Whether this computer may take the address `key` (`register`, `inbox --as`): it has a row
/// for it already, or no other computer of this person keeps it. Call it without `store`.
pub fn claim(d: &Daemon, key: &str) -> bool {
    let held = d.store.lock().unwrap().holds(key).unwrap_or(false);
    held || !kept_elsewhere(d, split_addr(key).map_or(key, |(_, r)| r))
}

/// A directory record (architecture §9); `cwd` is empty for another computer's session. This
/// computer (`own`) is verified, as its label is (`label::render`), and online; another member's
/// `online` is whether its computer is connected now.
fn record(m: &Member, me: &str, s: &Remote, cwd: &str, own: bool, online: bool) -> Value {
    json!({
        "session_id": s.session_id,
        "person": m.name,
        "person_verified": own || m.proof.is_some(),
        "computer": m.computer,
        "agent": s.agent,
        "cwd": cwd,
        "repo": s.repo,
        "branch": s.branch,
        "status": s.status,
        "doing": s.doing.clone().unwrap_or_default(),
        "title": s.title,
        "tier": s.tier,
        "online": own || online,
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
            workspace: None,
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
            out.push(record(&own, &me, &r, &s.cwd, true, true));
        }
    }
    let online = d.peers.online();
    for (m, l) in &remote_ones {
        let up = online.contains(&m.key);
        for r in l.sessions.iter().filter(|r| hit(m, r)) {
            out.push(record(m, &me, r, "", false, up));
        }
    }
    json!({ "sessions": out })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_older_presence_is_ignored() {
        let l = |seq| Latest {
            seq,
            seen_at: 1,
            sessions: vec![],
            kept: vec![format!("x/{seq}")],
        };
        let mut held = HashMap::new();
        let took: Vec<bool> = [5, 3, 5, 7]
            .into_iter()
            .map(|s| accept(&mut held, "m", l(s), false))
            .collect();
        assert_eq!(took, [true, false, false, true]);
        assert_eq!(held["m"], l(7));
        // A connection's first presence is taken whatever its seq.
        assert!(accept(&mut held, "m", l(2), true));
        assert_eq!(held["m"], l(2));
    }
}
