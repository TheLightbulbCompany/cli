//! Presence: this computer's sessions as its teammates see them, sent after each `hello` and
//! on change; the latest from each member (in memory, `peers_sessions` its copy across
//! restarts); and the `directory` op that merges both.

use crate::daemon::{ipc, Daemon, Declared};
use crate::mail;
use crate::net::frames::{Frame, Remote, STATUSES};
use crate::roster::{Member, Role, Roster};
use crate::sessions::{Session, Status};
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

/// The `agent` every member's latest presence declares, by session id (a workspace member's:
/// `Friday`), or a hosted app's title (`label::via`). One look per batch of labels.
pub fn declared_agents(d: &Daemon) -> HashMap<String, String> {
    let latest = d.latest.lock().unwrap();
    let all = latest.values().flat_map(|l| &l.sessions);
    all.map(|s| {
        let title = s.title.as_ref().filter(|t| crate::label::via(t));
        (s.session_id.clone(), title.unwrap_or(&s.agent).clone())
    })
    .collect()
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

/// `s` as published: `folder` is `cwd`'s last part, or all of it with `share_paths`; repo and
/// branch as its hook reported them.
fn remote(s: &Session, share_paths: bool) -> Remote {
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
        repo: s.repo.clone(),
        branch: s.branch.clone(),
        folder,
        status: STATUSES[usize::from(s.status != Status::Live)].into(),
        doing: s.doing.clone(),
        tier: s.tier.as_str().into(),
        title: s.title.clone(),
    }
    .clamp()
}

/// Whether `cwd` is under one of `off` (`folders_off`, each `canonical`). A string compare, no
/// filesystem call: a hook reports its cwd resolved (`resolved`), in the agent's process,
/// which has the folder access the person gave it.
pub(crate) fn hidden(off: &[PathBuf], cwd: &str) -> bool {
    let cwd = lower(PathBuf::from(cwd));
    off.iter().any(|o| cwd.starts_with(o))
}

/// `p` lowercased on macOS, whose volumes ignore case.
pub(crate) fn lower(p: PathBuf) -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from(p.to_string_lossy().to_lowercase())
    } else {
        p
    }
}

/// `p` resolved (symlinks, `..`); a path that does not exist (a deleted folder) is its nearest
/// existing parent resolved, then the rest as given.
pub fn resolved(p: &Path) -> PathBuf {
    p.ancestors()
        .find_map(|a| {
            let rest = p.strip_prefix(a).ok()?;
            Some(std::fs::canonicalize(a).ok()?.join(rest))
        })
        .unwrap_or_else(|| p.to_path_buf())
}

/// `resolved`, lowercased on macOS: how `folders_off` holds a folder.
pub(crate) fn canonical(p: &Path) -> PathBuf {
    lower(resolved(p))
}

/// This computer's presence: its live sessions (newest first), the non-live addresses still
/// holding mail (`known`, newest mail first) and the kept ones (`kept`, newest first); nothing
/// whose folder is under `folders_off`, nor an address whose folder it does not know.
/// An embedded instance's is the sessions it declared (`declared`).
fn build(d: &Daemon, seq: u64) -> Frame {
    if let Some(c) = &d.cloud {
        return declared(d, &c.declared(), seq);
    }
    let now = ipc::now();
    let (share, off) = {
        let t = d.team.lock().unwrap();
        (t.config.share_paths, t.off.clone())
    };
    let publishable = |cwd: &str| !hidden(&off, cwd);
    let mut live = ipc::live_sessions(d);
    let (rows, ended, waiting) = {
        let store = d.store.lock().unwrap();
        let since = now.saturating_sub(DAY) as i64;
        let live_keys = live.iter().map(|s| s.key.0.as_str()).collect();
        let _ = store.prune_hidden(since, &live_keys);
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
    // A session whose register a hidden folder refused has no row, but Claude's own file may
    // still name it (its folder spelled another way): its mark keeps that discovery from
    // teammates too (`hidden`). The mark is cleared by its next register, so after `folder
    // on` it stays hidden until its next hook event, or pruned a day after its last renewal
    // once no live session carries it; a discovery that never registered is matched by
    // Claude's cwd as given.
    live.retain(|s| !s.hidden);
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
    // The signed-in person's hosted apps' sessions, declared over the route link (`link`).
    let apps = crate::link::apps(d);
    Frame::Presence {
        member: d.identity.key.clone(),
        seq,
        sessions: live.iter().map(|s| remote(s, share)).chain(apps).collect(),
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

/// An embedded instance's presence: the sessions it declared (`cloud.sessions.set`), nothing
/// else.
fn declared(d: &Daemon, list: &Declared, seq: u64) -> Frame {
    Frame::Presence {
        member: d.identity.key.clone(),
        seq,
        sessions: list.sessions.to_vec(),
        known: vec![],
        kept: vec![],
    }
}

/// The sender, the only one: on a local change (`Daemon::presence`, coalesced over 250 ms) or
/// every 5 s, at most once a second and only while a member is online, builds this computer's
/// presence and sends it to every online member when it changed, and every 60 s regardless.
/// On an instance, each pass then reports the declared version it handed on (`Cloud::sent`),
/// which `cloud.sessions.set` waits for. Ends with the endpoint (started after it).
pub async fn run(d: Arc<Daemon>) {
    let Some(ep) = d.peers.endpoint.get().cloned() else {
        return;
    };
    let (mut built_at, mut sent_at): (Option<Instant>, Option<Instant>) = (None, None);
    loop {
        tokio::select! {
            _ = d.presence.notified() => tokio::time::sleep(COALESCE).await,
            _ = tokio::time::sleep(TICK) => {}
            _ = ep.closed() => return,
        }
        let list = d.cloud.as_ref().map(|c| c.declared());
        send_once(&d, list.as_ref(), &mut built_at, &mut sent_at).await;
        if let (Some(c), Some(l)) = (&d.cloud, list) {
            c.sent.send_replace(l.version);
        }
    }
}

/// One pass of `run`: an instance's presence is its `list`.
async fn send_once(
    d: &Arc<Daemon>,
    list: Option<&Declared>,
    built_at: &mut Option<Instant>,
    sent_at: &mut Option<Instant>,
) {
    if d.peers.online().is_empty() {
        return;
    }
    if let Some(at) = *built_at {
        tokio::time::sleep(GAP.saturating_sub(at.elapsed())).await;
    }
    *built_at = Some(Instant::now());
    let mut f = match list {
        Some(l) => declared(d, l, 0),
        None => {
            let d2 = d.clone();
            match tokio::task::spawn_blocking(move || build(&d2, 0)).await {
                Ok(f) => f,
                Err(_) => return,
            }
        }
    };
    let unchanged = d
        .built
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|b| same(b, &f));
    *d.built.lock().unwrap() = Some(f.clone());
    if unchanged && sent_at.is_some_and(|at| at.elapsed() < RESEND) {
        return;
    }
    // Stamped at send, after the store: a later send (here or a hello's) carries a later seq.
    stamp(&mut f);
    for m in d.peers.online() {
        let _ = d.peers.send(&m, f.clone());
    }
    *sent_at = Some(Instant::now());
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

/// A directory record (architecture §9); `cwd` is empty for another computer's session.
/// `verified` as its label says it (`label::verified`); `online`, whether its computer is
/// connected now (this one always is).
fn record(m: &Member, me: &str, s: &Remote, cwd: &str, verified: bool, online: bool) -> Value {
    json!({
        "session_id": s.session_id,
        "person": m.name,
        "person_id": m.person_id,
        "person_verified": verified,
        "computer": m.computer,
        "agent": s.agent,
        "cwd": cwd,
        "repo": s.repo,
        "branch": s.branch,
        "status": s.status,
        "doing": s.doing.clone().unwrap_or_default(),
        "title": s.title,
        "tier": s.tier,
        "online": online,
        "can_send": true,
        "needs_owner_ok": m.person_id != me,
    })
}

/// `directory {query?}` -> `{sessions}`: this computer's sessions, then every member's latest
/// presence; `query` matches session id, person, computer, agent, folder (and this computer's
/// full path), repo, branch, what it is doing or its title, any case.
pub fn directory(d: &Daemon, query: Option<&str>) -> Value {
    // This computer's, with their folders; an instance's are the sessions it declared.
    let local: Vec<(Remote, String, Option<String>, bool)> = match &d.cloud {
        Some(c) => c
            .declared()
            .sessions
            .iter()
            .map(|r| (r.clone(), String::new(), None, false))
            .collect(),
        None => ipc::live_sessions(d)
            .iter()
            .map(|s| (remote(s, false), s.cwd.clone(), s.handle.clone(), s.hidden))
            .chain(
                crate::link::apps(d)
                    .into_iter()
                    .map(|r| (r, String::new(), None, false)),
            )
            .collect(),
    };
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
            admitted_at: 0,
            by: None,
        });
        (my_person(d, &t.roster), own)
    };
    let q = query.map(str::to_lowercase).filter(|q| !q.is_empty());
    let hit = |m: &Member, s: &Remote, cwd: &str| {
        q.as_ref().is_none_or(|q| {
            [&s.session_id, &m.name, &m.computer, &s.agent, &s.folder]
                .into_iter()
                .map(String::as_str)
                .chain(
                    [&s.repo, &s.branch, &s.doing, &s.title]
                        .into_iter()
                        .flatten()
                        .map(String::as_str),
                )
                .chain([cwd])
                .any(|f| f.to_lowercase().contains(q))
        })
    };
    let key = &d.identity.key;
    let mut out = vec![];
    for (r, cwd, handle, hidden) in &local {
        // `who`'s name for it (`claude-5`) finds it too: people pass it on to their agents.
        let named = |q: &String| {
            handle
                .as_deref()
                .is_some_and(|h| h.to_lowercase().contains(q))
        };
        if hit(&own, r, cwd) || q.as_ref().is_some_and(named) {
            let mut v = record(&own, &me, r, cwd, true, true);
            v["handle"] = handle.clone().into();
            if *hidden {
                v["hidden"] = true.into();
            }
            out.push(v);
        }
    }
    for (m, l) in &remote_ones {
        let (verified, up) = (crate::label::verified(m, key), d.peers.is_online(&m.key));
        for r in l.sessions.iter().filter(|r| hit(m, r, "")) {
            out.push(record(m, &me, r, "", verified, up));
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
