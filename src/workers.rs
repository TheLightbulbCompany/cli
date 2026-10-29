//! Network workers, host side: what this computer offers and whom it lets start a worker; the
//! start, the registry of running workers, their prompts, stop, deadline and reap, and the
//! result each sends from its own key.

mod driver;

pub use driver::{
    driver_for, launchable, Decision, Driver, Outcome, PermissionRequest, Relay, Run,
};

use crate::config::{expand_home, Config};
use crate::daemon::{ipc, Daemon, TeamState};
use crate::holds::{self, signed_record};
use crate::identity::Identity;
use crate::mail::{self, Meta, State};
use crate::sessions::{self, Key, PROVIDERS};
use crate::store::{Message, Store, Worker};
use crate::{frame, label, Result};
use rusqlite::params;
use std::collections::HashMap;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

/// What a member may do here: start a worker without asking, be asked, or be refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Allow,
    Ask,
    Refuse,
}

/// `member`'s policy: the latest standing record, else this computer's own key may, others ask.
pub fn policy(store: &Store, me: &str, member: &str) -> rusqlite::Result<Policy> {
    let scope: Option<String> = store
        .conn()
        .query_row(
            "SELECT scope FROM approvals WHERE hold_id IS NULL \
             AND scope IN ('always', 'refused', 'revoked') \
             AND json_extract(record, '$.member') = ?1 ORDER BY rowid DESC LIMIT 1",
            [member],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e),
        })?;
    Ok(match scope.as_deref() {
        Some("always") => Policy::Allow,
        Some("refused") => Policy::Refuse,
        Some("revoked") => Policy::Ask,
        _ if member == me => Policy::Allow,
        _ => Policy::Ask,
    })
}

/// Records `scope` (`always`, `refused` or `revoked`) for `member`, signed by this computer.
pub fn set_policy(
    store: &Store,
    me: &Identity,
    member: &str,
    scope: &str,
    by: &str,
    now: i64,
) -> Result<()> {
    let verdict = match scope {
        "always" => "allow",
        "refused" => "decline",
        "revoked" => "revoke",
        _ => return Err(format!("no worker policy {scope}").into()),
    };
    let (nonce, record) = signed_record(
        me,
        serde_json::json!({"member": member, "scope": scope, "by": by, "decided_at": now}),
    );
    store.conn().execute(
        "INSERT INTO approvals VALUES (NULL, ?1, ?2, ?3, ?4, ?5, ?6)",
        params![verdict, scope, by, nonce, now, record],
    )?;
    Ok(())
}

/// The harnesses `workers_offer` names that this build knows.
pub fn offers(config: &Config) -> Vec<String> {
    config
        .workers_offer
        .split(',')
        .map(str::trim)
        .filter(|p| PROVIDERS.contains(p))
        .map(String::from)
        .collect()
}

/// The folder workers run in (`~` expanded).
pub fn folder(config: &Config) -> PathBuf {
    expand_home(&config.workers_folder)
}

/// What `take_assign` did with an assignment addressed to a fresh worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Taken {
    /// Nothing written; the asker is told this reason (`no_worker`, `busy`, `declined`).
    Refused(&'static str),
    /// Stored, receipt `held`, with this open `holds` row: the caller shows its dialog.
    Held { hold: i64 },
    /// Stored, receipt `stored`: the caller runs `start`.
    Start,
}

/// Decides assignment `assign` (to `<member>/<provider>:new`) on this computer `me` (member
/// short key): refused when this computer does not offer the harness, is running `workers_max`
/// workers, or refuses the asker; else stored (in no mailbox) and either held for the person
/// (their policy for the asker is Ask) or ready to start. The caller holds the store lock and
/// a transaction.
pub fn take_assign(
    store: &Store,
    config: &Config,
    me: &str,
    assign: &Message,
    now: i64,
) -> Result<Taken> {
    let provider = sessions::split_addr(&assign.to_key)
        .and_then(|(_, rest)| rest.strip_suffix(":new"))
        .unwrap_or_default();
    if !offers(config).iter().any(|p| p == provider) {
        return Ok(Taken::Refused("no_worker"));
    }
    if store.workers(true)?.len() as i64 >= config.workers_max {
        return Ok(Taken::Refused("busy"));
    }
    let member = sessions::member_of(&assign.from_key).unwrap_or_default();
    let verdict = policy(store, me, member)?;
    if verdict == Policy::Refuse {
        return Ok(Taken::Refused("declined"));
    }
    store.put_message(assign)?;
    let (from, rid, at) = (&assign.from_key, &assign.request_id, now as u64);
    if verdict == Policy::Allow {
        mail::advance(store.conn(), from, rid, State::Stored, None, None, at)?;
        return Ok(Taken::Start);
    }
    mail::advance(store.conn(), from, rid, State::Held, None, None, at)?;
    let hold = holds::insert(store, assign.clone(), member, now)?;
    Ok(Taken::Held { hold: hold.id })
}

/// The workers this daemon runs, by `workers` row id, and the worker prompts waiting on a
/// person, by hold id.
#[derive(Default)]
pub struct Registry {
    running: Mutex<HashMap<i64, Running>>,
    prompts: Mutex<HashMap<i64, oneshot::Sender<Decision>>>,
}

/// Why a worker is stopped, and by whom.
type Stop = (&'static str, String);

/// A running worker: its process (group) and the channel that tells its task why it stops.
pub struct Running {
    pub pid: u32,
    pub pgid: i32,
    /// Taken by the first `stop`.
    stop: Option<oneshot::Sender<Stop>>,
}

impl Registry {
    /// Hold `hold` closed with `verdict` (`allow`, `decline`, `expired`; `by` none for an
    /// expiry): a worker waiting on it hears the answer.
    pub fn answered(&self, hold: i64, verdict: &str, by: Option<&str>) {
        let Some(reply) = self.prompts.lock().unwrap().remove(&hold) else {
            return;
        };
        let _ = reply.send(match (verdict, by) {
            (holds::ALLOW, _) => Decision::Allow,
            (_, Some(by)) => Decision::Deny(format!("declined by {by}")),
            (_, None) => Decision::Deny("the prompt expired unanswered".into()),
        });
    }

    fn insert(&self, id: i64, pid: u32) -> oneshot::Receiver<Stop> {
        let (tx, rx) = oneshot::channel();
        let run = Running {
            pid,
            pgid: pid as i32, // `process_group(0)`: the child leads its own group
            stop: Some(tx),
        };
        self.running.lock().unwrap().insert(id, run);
        rx
    }
}

/// The provider of a `<member>/<provider>:new` address.
fn provider_of(to: &str) -> &str {
    sessions::split_addr(to)
        .and_then(|(_, rest)| rest.strip_suffix(":new"))
        .unwrap_or_default()
}

/// Creates `dir` (and its parents) `0700` when it does not exist.
fn private_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    Ok(())
}

/// Request `rid`'s folder, `<base>/<rid>/`, private (`0700`): a git worktree on branch
/// `meta.branch` (else `lb/<rid>`) when `meta.repo` names a clone right under `base`, else
/// empty. No fetch; a worktree git refuses leaves the folder empty.
pub fn workspace(base: &Path, rid: &str, meta: &Meta) -> Result<PathBuf> {
    private_dir(base)?;
    let dir = base.join(rid);
    // One plain name: never a path out of `base`.
    let clone = (meta.repo.as_deref())
        .filter(|r| sessions::word(r, 256) && !r.starts_with('.'))
        .map(|r| base.join(r))
        .filter(|c| c.join(".git").exists());
    if let (Some(clone), false) = (clone, dir.exists()) {
        let branch = meta.branch.clone().unwrap_or_else(|| format!("lb/{rid}"));
        let made = std::process::Command::new("git")
            .arg("-C")
            .arg(&clone)
            .args(["worktree", "add", "-q", "-b", &branch])
            .arg(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !made {
            eprintln!("worker {rid}: no worktree of its repo; an empty folder");
        }
    }
    private_dir(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// The worker's first prompt: assignment `a` framed as an agent gets it from `label`, then
/// how to answer it.
pub fn first_prompt(label: &str, a: &Message) -> String {
    let framed = frame::framed_kind("assign", label, &a.request_id, None, &a.meta, &a.body);
    format!(
        "{framed}\nReply with send_message(reply_to=\"{}\") when done (status failed or \
         cancelled if not).",
        a.request_id
    )
}

/// `start`'s half under the store lock, the one place the cap is kept: assignment `a`, not yet
/// claimed and before its deadline, gets a `starting` row as `session` while fewer than
/// `workers_max` run; at the cap its receipt is `refused`/`busy` (a hold it came through stays
/// closed as it is). The row, or `None` when nothing is to start.
pub fn claim(
    store: &Store,
    config: &Config,
    a: &Message,
    session: &str,
    now: i64,
) -> Result<Option<Worker>> {
    let late = a.meta.deadline_at.is_none_or(|d| now >= d);
    if late || store.worker_by_assignment(&a.id)?.is_some() {
        return Ok(None);
    }
    if store.workers(true)?.len() as i64 >= config.workers_max {
        let (from, rid, at) = (&a.from_key, &a.request_id, now as u64);
        mail::advance(
            store.conn(),
            from,
            rid,
            State::Refused,
            Some("busy"),
            None,
            at,
        )?;
        return Ok(None);
    }
    let mut w = Worker {
        id: 0,
        assignment: a.id.clone(),
        session: session.into(),
        provider: provider_of(&a.to_key).into(),
        folder: folder(config).join(&a.request_id),
        pid: None,
        started_at: now,
        ended_at: None,
        state: "starting".into(),
        by: None,
    };
    w.id = store.insert_worker(&w)?;
    Ok(Some(w))
}

/// Starts the worker for stored assignment `assign_id` (called with no lock held): claims it
/// (`claim`), makes its folder (`workspace`), registers its session `<me>/<provider>:<fresh
/// id>` and spawns its harness; a task (`drive`) hands it the first prompt and sends its
/// result. Nothing starts at the cap, or without a driver for the harness (`no_worker`).
pub fn start(d: &Arc<Daemon>, assign_id: &str) -> Result<()> {
    let Some(a) = d.store.lock().unwrap().message(assign_id)? else {
        return Err(format!("no assignment {assign_id}").into());
    };
    let provider = provider_of(&a.to_key).to_string();
    let now = ipc::now() as i64;
    let Some(driver) = driver_for(&provider) else {
        let (from, rid, at) = (&a.from_key, &a.request_id, now as u64);
        let refused = Some("no_worker");
        let store = d.store.lock().unwrap();
        mail::advance(store.conn(), from, rid, State::Refused, refused, None, at)?;
        drop(store);
        d.peers.outbox.notify_one();
        eprintln!("worker for {assign_id}: no {provider} driver");
        return Ok(());
    };
    let label = ipc::sender_label(d, &a); // takes the team lock
    let conv = crate::telemetry::uuid_v4();
    let key = Key::new(&d.identity.short, &provider, &conv);
    let (claimed, base, asker) = {
        let team = d.team.lock().unwrap();
        let store = d.store.lock().unwrap();
        let member = sessions::member_of(&a.from_key).unwrap_or_default();
        (
            claim(&store, &team.config, &a, &key.0, now)?,
            folder(&team.config),
            team.roster.by_short(member).map(label::name),
        )
    };
    let Some(w) = claimed else {
        d.peers.outbox.notify_one(); // a `busy` refusal goes to its asker
        eprintln!("worker for {assign_id}: not started");
        return Ok(());
    };
    let spawned = workspace(&base, &a.request_id, &a.meta).and_then(|dir| {
        let argv = driver.argv(&dir, &conv);
        Ok((driver::spawn(&argv, &dir)?, dir, argv))
    });
    let (child, dir, argv) = match spawned {
        Ok(s) => s,
        Err(e) => {
            eprintln!("worker {}: not started: {e}", w.id);
            let o = Outcome {
                status: "failed",
                reason: Some("error"),
                body: "the worker could not be started".into(),
            };
            finish(d, &w, o, None);
            return Ok(());
        }
    };
    let pid = child.id().unwrap_or_default();
    let stop = d.workers.insert(w.id, pid);
    {
        let store = d.store.lock().unwrap();
        let birth = crate::os::process(pid).ok().map(|p| p.birth_micros as i64);
        store.put_session(&crate::store::Session {
            key: key.0.clone(),
            provider,
            conversation: Some(conv),
            pid: Some(pid.into()),
            birth_micros: birth,
            cwd: dir.to_string_lossy().into(),
            title: Some(format!(
                "{}'s worker",
                asker.as_deref().unwrap_or("a teammate")
            )),
            status: "live".into(),
            seen_at: now,
            executable: argv.first().cloned(),
            repo: None,
            branch: None,
            handle: None,
        })?;
        store.set_worker_running(w.id, pid)?;
    }
    d.presence.notify_one();
    eprintln!("worker {}: running pid={pid} for {assign_id}", w.id);
    let prompt = first_prompt(&label, &a);
    tokio::spawn(drive(d.clone(), w, driver, child, prompt, stop));
    Ok(())
}

/// Runs worker `w`'s driver to its end: each permission prompt becomes a hold on the worker's
/// session (`prompt`), a conversation id the harness names re-keys it (`rekey`), a stop is
/// remembered; then its result goes out (`finish`).
async fn drive(
    d: Arc<Daemon>,
    mut w: Worker,
    driver: Box<dyn Driver>,
    child: tokio::process::Child,
    first_prompt: String,
    mut stop: oneshot::Receiver<Stop>,
) {
    let (permission, mut asks) = tokio::sync::mpsc::channel(8);
    let (session, mut named) = oneshot::channel();
    let run = driver.run(
        child,
        first_prompt,
        Relay {
            permission,
            session,
        },
    );
    tokio::pin!(run);
    let (mut stopped, mut stop_seen, mut named_seen) = (None, false, false);
    let outcome = loop {
        tokio::select! {
            o = &mut run => break o,
            Some(ask) = asks.recv() => prompt(&d, &w, ask),
            r = &mut named, if !named_seen => {
                named_seen = true;
                if let Ok(conv) = r {
                    rekey(&d, &mut w, &conv);
                }
            }
            r = &mut stop, if !stop_seen => {
                stop_seen = true;
                stopped = r.ok();
            }
        }
    };
    // A stop that raced the harness's own end still names why it ended.
    if !stop_seen {
        stopped = stop.try_recv().ok();
    }
    finish(&d, &w, outcome, stopped);
}

/// Files worker `w`'s prompt `ask` as a permission hold on its session and shows its dialog;
/// the hold's close answers it (`Registry::answered`).
fn prompt(d: &Arc<Daemon>, w: &Worker, ask: PermissionRequest) {
    let team = d.team.lock().unwrap();
    let store = d.store.lock().unwrap();
    let text = format!("{}: {}", ask.tool, ask.summary);
    match holds::file_permission(&store, &w.session, &text, ipc::now() as i64) {
        Ok(h) => {
            d.workers.prompts.lock().unwrap().insert(h.id, ask.reply);
            holds::shown(d, &team, &store, &h);
            eprintln!("worker {}: prompt {}", w.id, h.name());
        }
        Err(e) => {
            eprintln!("worker {}: prompt not filed: {e}", w.id);
            let _ = ask
                .reply
                .send(Decision::Deny("the person could not be asked".into()));
        }
    }
}

/// The harness named its own conversation id `conv` (Codex): the worker's session is
/// `<me>/<provider>:<conv>` from now on.
fn rekey(d: &Daemon, w: &mut Worker, conv: &str) {
    let key = Key::new(&d.identity.short, &w.provider, conv);
    let store = d.store.lock().unwrap();
    let moved = (|| -> rusqlite::Result<()> {
        let c = store.conn();
        c.execute(
            "UPDATE sessions SET key = ?2, conversation = ?3 WHERE key = ?1",
            params![w.session, key.0, conv],
        )?;
        c.execute(
            "UPDATE workers SET session = ?2 WHERE id = ?1",
            params![w.id, key.0],
        )?;
        Ok(())
    })();
    match moved {
        Ok(()) => w.session = key.0,
        Err(e) => eprintln!("worker {}: not re-keyed: {e}", w.id),
    }
}

/// `who` as the asker reads it: this computer's person for a local surface, else as given.
fn named(d: &Daemon, team: &TeamState, who: &str) -> String {
    match who {
        "local" | "cli" | "dialog" => team
            .roster
            .by_short(&d.identity.short)
            .map(label::name)
            .unwrap_or_else(|| who.into()),
        _ => who.into(),
    }
}

/// Worker `w` ended with `o` (or was `stopped`): its result goes to the asker from the
/// worker's own key, unless the worker already answered; its row and session end; its open
/// prompts close `expired`.
fn finish(d: &Arc<Daemon>, w: &Worker, o: Outcome, stopped: Option<Stop>) {
    let (status, reason, state) = match &stopped {
        Some(("cancelled", _)) => ("cancelled", None, "cancelled"),
        Some(("stopped", _)) => ("failed", Some("stopped"), "stopped"),
        Some((r, _)) => ("failed", Some(*r), "failed"),
        None => (o.status, o.reason, o.status),
    };
    let by = stopped.as_ref().map(|(_, by)| by.as_str());
    let mut body = match &stopped {
        Some(("stopped", who)) => {
            let team = d.team.lock().unwrap();
            let line = format!(
                "stopped by {} on {}",
                named(d, &team, who),
                team.config.computer
            );
            format!("{line}\n{}", o.body)
        }
        _ => o.body,
    };
    if body.len() > mail::MAX_TEXT {
        let mut n = mail::MAX_TEXT;
        while !body.is_char_boundary(n) {
            n -= 1;
        }
        body.truncate(n);
    }
    let now = ipc::now();
    let prepared = (|| -> Result<Option<mail::Prepared>> {
        let store = d.store.lock().unwrap();
        let Some(a) = store.message(&w.assignment)? else {
            return Ok(None);
        };
        if store.result_of(&a.id)?.is_some() {
            return Ok(None); // the worker answered it itself
        }
        let meta = Meta {
            status: Some(status.into()),
            reason: reason.map(Into::into),
            ..Meta::default()
        };
        let rid = format!("result-{}", a.id);
        let from = Key(w.session.clone());
        let p = mail::prepare(
            &store,
            &from,
            None,
            &body,
            &rid,
            Some(&a.request_id),
            "result",
            meta,
            now,
        )?;
        Ok(match p {
            mail::Prep::Go(p) => Some(p),
            mail::Prep::Done(r) => {
                eprintln!("worker {}: result {:?} {:?}", w.id, r.state, r.reason);
                None
            }
        })
    })();
    match prepared {
        Ok(Some(p)) => {
            if let Err(e) = ipc::dispatch(d, &p) {
                eprintln!("worker {}: result not sent: {e}", w.id);
            }
        }
        Ok(None) => {}
        Err(e) => eprintln!("worker {}: result not sent: {e}", w.id),
    }
    let closed = {
        let store = d.store.lock().unwrap();
        let now = now as i64;
        let ended = (store.end_worker(w.id, state, by, now))
            .and_then(|()| store.end_session(&w.session, now).map(drop));
        if let Err(e) = ended {
            eprintln!("worker {}: not ended: {e}", w.id);
        }
        holds::expire_for(&store, &w.session).unwrap_or_default()
    };
    for h in closed {
        holds::closed(d, &h, "expired", None, None);
    }
    d.workers.running.lock().unwrap().remove(&w.id);
    d.presence.notify_one();
    eprintln!("worker {}: {state} ({} bytes)", w.id, body.len());
}

/// Stops worker `worker_id` for `reason` (`stopped`, `cancelled`, `timed_out`) by `by`: its
/// task is told why, its process group gets SIGTERM, and SIGKILL 5 s later if any of it is
/// left. A second stop of the same worker does nothing; one not running here is an error.
pub fn stop(d: &Arc<Daemon>, worker_id: i64, reason: &'static str, by: &str) -> Result<()> {
    let (pgid, tell) = {
        let mut running = d.workers.running.lock().unwrap();
        let Some(r) = running.get_mut(&worker_id) else {
            return Err(format!("worker {worker_id} is not running here").into());
        };
        (r.pgid, r.stop.take())
    };
    let Some(tell) = tell else {
        return Ok(());
    };
    let _ = tell.send((reason, by.into()));
    // SAFETY: plain syscalls on a process group id.
    unsafe { libc::killpg(pgid, libc::SIGTERM) };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        // ponytail: a group id is reused only once all of its processes are gone and a new
        // leader gets that pid; within 5 s that is not a risk worth a pidfd.
        if unsafe { libc::killpg(pgid, 0) } == 0 {
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
    });
    eprintln!("worker {worker_id}: stopping ({reason})");
    Ok(())
}

/// A person asked to stop worker `session` (the dialog's Stop worker).
pub fn stop_requested(d: &Arc<Daemon>, session: &str, by: &str) {
    let running = d.store.lock().unwrap().worker_by_session(session);
    if let Ok(Some(w)) = running {
        if w.ended_at.is_none() {
            if let Err(e) = stop(d, w.id, "stopped", by) {
                eprintln!("worker {}: {e}", w.id);
            }
        }
    }
}

/// The asker `by` cancelled assignment `assign_id` addressed to a fresh worker: an open start
/// hold is declined and its dialog withdrawn; a running worker is stopped (`cancelled`); a row
/// with nothing running here ends `cancelled`.
pub fn cancel(d: &Arc<Daemon>, assign_id: &str, by: &str) -> Result<()> {
    let now = ipc::now() as i64;
    let (declined, worker) = {
        let store = d.store.lock().unwrap();
        let ids: Vec<i64> = {
            let mut stmt = (store.conn())
                .prepare("SELECT id FROM holds WHERE message_id = ?1 AND state = 'open'")?;
            let rows = stmt.query_map([assign_id], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut declined = vec![];
        for id in ids {
            (store.conn()).execute("UPDATE holds SET state = 'declined' WHERE id = ?1", [id])?;
            declined.extend(holds::get(&store, id)?);
        }
        let w = store.worker_by_assignment(assign_id)?;
        (declined, w.filter(|w| w.ended_at.is_none()))
    };
    for h in &declined {
        holds::closed(d, h, holds::DECLINE, Some(by), None);
    }
    if let Some(w) = worker {
        if stop(d, w.id, "cancelled", by).is_err() {
            d.store
                .lock()
                .unwrap()
                .end_worker(w.id, "cancelled", Some(by), now)?;
        }
    }
    eprintln!("worker for {assign_id}: cancelled");
    Ok(())
}

/// The running workers whose assignment's deadline is at or before `now` are stopped
/// (`timed_out`); from the 60 s mail timer.
pub fn tick(d: &Arc<Daemon>, now: i64) {
    let due: Vec<i64> = {
        let store = d.store.lock().unwrap();
        let past = |w: &Worker| {
            let a = store.message(&w.assignment).ok().flatten();
            a.and_then(|a| a.meta.deadline_at).is_some_and(|t| t <= now)
        };
        let running = store.workers(true).unwrap_or_default();
        running.into_iter().filter(past).map(|w| w.id).collect()
    };
    for id in due {
        if d.workers.running.lock().unwrap().contains_key(&id) {
            let _ = stop(d, id, "timed_out", "deadline");
        }
    }
}

/// Whether worker `w`'s process is still the one it started: its session row's pid and birth,
/// else its pid alone.
fn alive(store: &Store, w: &Worker) -> bool {
    let row = store.session(&w.session).ok().flatten();
    match row.and_then(|s| {
        Some((
            u32::try_from(s.pid?).ok()?,
            u64::try_from(s.birth_micros?).ok()?,
        ))
    }) {
        Some((pid, birth)) => sessions::alive(pid, birth),
        None => w.pid.is_some_and(|p| crate::os::process(p).is_ok()),
    }
}

/// `reap`'s store half: each unended worker whose process is gone ends `failed` (by `daemon
/// restarted`) with its session, and its open prompts close `expired`. The holds closed, and
/// the workers still alive (to re-adopt).
pub fn reaped(store: &Store, now: i64) -> Result<(Vec<holds::Hold>, Vec<Worker>)> {
    let (mut closed, mut alive_ones) = (vec![], vec![]);
    for w in store.workers(true)? {
        if alive(store, &w) {
            alive_ones.push(w);
            continue;
        }
        store.end_worker(w.id, "failed", Some("daemon restarted"), now)?;
        store.end_session(&w.session, now)?;
        closed.extend(holds::expire_for(store, &w.session)?);
    }
    Ok((closed, alive_ones))
}

/// At daemon start: the workers whose process is gone are ended (`reaped`) and their dialogs
/// withdrawn; one still running (its own process group outlived the old daemon) is re-adopted
/// with a stop channel and no stream: stopped, or found gone, it ends `failed` with what is
/// known (its output went to the old daemon).
pub fn reap(d: &Arc<Daemon>, now: i64) {
    let r = reaped(&d.store.lock().unwrap(), now);
    let (closed, adopted) = match r {
        Ok(r) => r,
        Err(e) => return eprintln!("workers: reap: {e}"),
    };
    for h in closed {
        holds::closed(d, &h, "expired", None, None);
    }
    for w in adopted {
        let Some(pid) = w.pid else { continue };
        let mut stop = d.workers.insert(w.id, pid);
        eprintln!("worker {}: re-adopted pid={pid}", w.id);
        let d = d.clone();
        tokio::spawn(async move {
            let gone = || unsafe { libc::kill(pid as i32, 0) } != 0;
            let stopped = loop {
                tokio::select! {
                    r = &mut stop => break r.ok(),
                    _ = tokio::time::sleep(Duration::from_secs(5)) => if gone() { break None },
                }
            };
            for _ in 0..12 {
                if gone() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            let body = "the worker's output went to a daemon that has since restarted".into();
            let o = Outcome {
                status: "failed",
                reason: Some("error"),
                body,
            };
            finish(&d, &w, o, stopped);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assign(id: &str, rid: &str, from: &str, deadline: i64) -> Message {
        Message {
            id: id.into(),
            request_id: rid.into(),
            from_key: from.into(),
            to_key: "m/claude:new".into(),
            kind: "assign".into(),
            thread: None,
            hops: 0,
            origin: None,
            body: "do it".into(),
            sent_at: 1,
            reply_to: None,
            meta: Meta {
                done_when: Some("tests pass".into()),
                deadline_at: Some(deadline),
                ..Meta::default()
            },
        }
    }

    fn mode(p: &std::path::Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-C"])
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn a_worker_folder_is_private_and_per_request() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("work");
        let f = workspace(&base, "r1", &Meta::default()).unwrap();
        assert_eq!(f, base.join("r1"));
        assert_eq!((mode(&base), mode(&f)), (0o700, 0o700));
        // A second request gets its own folder.
        assert_eq!(
            workspace(&base, "r2", &Meta::default()).unwrap(),
            base.join("r2")
        );
    }

    #[test]
    fn a_repo_hint_makes_a_worktree_of_a_clone_under_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("work");
        let clone = base.join("app");
        std::fs::create_dir_all(&clone).unwrap();
        git(&clone, &["init", "-q"]);
        git(&clone, &["commit", "-q", "--allow-empty", "-m", "first"]);
        let hint = |repo: &str| Meta {
            repo: Some(repo.into()),
            ..Meta::default()
        };
        let f = workspace(&base, "r1", &hint("app")).unwrap();
        assert_eq!(mode(&f), 0o700);
        let list = git(&clone, &["worktree", "list"]);
        let shown = f.canonicalize().unwrap();
        assert!(
            list.lines()
                .any(|l| l.starts_with(&*shown.to_string_lossy()) && l.ends_with("[lb/r1]")),
            "{list}"
        );
        // No clone of that name (nor one outside the folder): an empty folder.
        for repo in ["nope", "../app", ".."] {
            let f = workspace(&base, "r2", &hint(repo)).unwrap();
            assert_eq!(std::fs::read_dir(&f).unwrap().count(), 0, "{repo}");
        }
    }

    #[test]
    fn the_first_prompt_is_the_assignment_frame() {
        let p = first_prompt("Aden's Codex", &assign("a1", "r1", "p/codex:a", 100));
        assert!(
            p.starts_with("[Lightbulb assignment from Aden's Codex"),
            "{p}"
        );
        assert!(p.contains("\"doneWhen\""), "{p}");
        assert!(p.ends_with(
            "\nReply with send_message(reply_to=\"r1\") when done (status failed or cancelled if not)."
        ));
    }

    /// A worker of this process for `assign_id`, as session `key`; `alive` picks this
    /// process's real birth time, else a wrong one.
    fn worker_row(s: &Store, assign_id: &str, key: &str, alive: bool) -> i64 {
        let me = std::process::id();
        let birth = crate::os::process(me).unwrap().birth_micros as i64;
        s.put_session(&crate::store::Session {
            key: key.into(),
            provider: "claude".into(),
            conversation: Some("w".into()),
            pid: Some(me.into()),
            birth_micros: Some(if alive { birth } else { 1 }),
            cwd: "/".into(),
            title: None,
            status: "live".into(),
            seen_at: 0,
            executable: None,
            repo: None,
            branch: None,
            handle: None,
        })
        .unwrap();
        s.insert_worker(&Worker {
            id: 0,
            assignment: assign_id.into(),
            session: key.into(),
            provider: "claude".into(),
            folder: "/f".into(),
            pid: Some(me),
            started_at: 1,
            ended_at: None,
            state: "running".into(),
            by: None,
        })
        .unwrap()
    }

    #[test]
    fn a_restarted_daemon_reaps_a_worker_whose_process_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("state.db")).unwrap();
        let gone = worker_row(&s, "a1", "m/claude:w1", false);
        let live = worker_row(&s, "a2", "m/claude:w2", true);
        let h = holds::file_permission(&s, "m/claude:w1", "Bash: rm -rf x", 5).unwrap();
        let (closed, adopted) = reaped(&s, 10).unwrap();
        let w = s.worker_by_assignment("a1").unwrap().unwrap();
        assert_eq!(
            (w.id, w.state.as_str(), w.by.as_deref(), w.ended_at),
            (gone, "failed", Some("daemon restarted"), Some(10))
        );
        assert_eq!(holds::get(&s, h.id).unwrap().unwrap().state, "expired");
        assert_eq!(closed.iter().map(|h| h.id).collect::<Vec<_>>(), [h.id]);
        assert!(
            s.session("m/claude:w1").unwrap().is_none(),
            "its session ended"
        );
        // The live one is re-adopted, untouched.
        assert_eq!(adopted.iter().map(|w| w.id).collect::<Vec<_>>(), [live]);
        assert_eq!(
            s.worker_by_assignment("a2").unwrap().unwrap().state,
            "running"
        );
    }

    const BYPASS: [&str; 4] = [
        "--dangerously-skip-permissions",
        "--dangerously-bypass-approvals-and-sandbox",
        "bypassPermissions",
        "--approve-for-me",
    ];

    /// A driver whose argv is fixed.
    struct Fake(Vec<String>);

    impl Driver for Fake {
        fn argv(&self, _folder: &Path, _session_id: &str) -> Vec<String> {
            self.0.clone()
        }
        fn run(self: Box<Self>, _c: tokio::process::Child, _p: String, _r: Relay) -> Run {
            Box::pin(async {
                Outcome {
                    status: "done",
                    reason: None,
                    body: String::new(),
                }
            })
        }
    }

    #[test]
    fn no_launch_argv_contains_a_bypass_flag() {
        let folder = Path::new("/w/r1");
        for provider in ["claude", "codex"] {
            if let Some(d) = driver_for(provider) {
                let argv = d.argv(folder, "s1");
                assert!(launchable(&argv), "{provider}: {argv:?}");
            }
        }
        assert!(launchable(
            &Fake(vec!["claude".into(), "-p".into()]).argv(folder, "s")
        ));
        for flag in BYPASS {
            let argv = vec!["claude".into(), format!("--permission-mode={flag}")];
            assert!(!launchable(&argv), "{flag}");
            assert!(!launchable(&["x".into(), flag.into()]), "{flag}");
        }
    }

    #[test]
    fn start_refuses_busy_under_the_cap_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("state.db")).unwrap();
        let config = Config {
            workers_max: 1,
            ..Config::default()
        };
        let (a, b) = (
            assign("a1", "r1", "p/codex:a", 100),
            assign("a2", "r2", "q/codex:b", 100),
        );
        for m in [&a, &b] {
            s.put_message(m).unwrap();
            mail::advance(
                s.conn(),
                &m.from_key,
                &m.request_id,
                State::Stored,
                None,
                None,
                1,
            )
            .unwrap();
        }
        let first = claim(&s, &config, &a, "m/claude:w1", 10).unwrap().unwrap();
        assert_eq!(
            (first.state.as_str(), first.session.as_str()),
            ("starting", "m/claude:w1")
        );
        assert!(claim(&s, &config, &b, "m/claude:w2", 10).unwrap().is_none());
        let r = mail::receipt(&s, &crate::sessions::Key(b.from_key.clone()), "r2")
            .unwrap()
            .unwrap();
        assert_eq!(
            (r.state, r.reason.as_deref()),
            (State::Refused, Some("busy"))
        );
        assert_eq!(s.workers(true).unwrap().len(), 1);
        // Claimed once: a second start of the same assignment starts nothing.
        let config = Config {
            workers_max: 3,
            ..config
        };
        assert!(claim(&s, &config, &a, "m/claude:w3", 11).unwrap().is_none());
        // Past its deadline: not started.
        let late = assign("a3", "r3", "p/codex:a", 5);
        s.put_message(&late).unwrap();
        assert!(claim(&s, &config, &late, "m/claude:w4", 10)
            .unwrap()
            .is_none());
    }

    #[test]
    fn policy_defaults_and_latest_record_wins() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        let me = Identity::load_or_create(dir.path()).unwrap();
        let (mine, bob) = (me.key.as_str(), "bobkey");
        assert_eq!(policy(&store, mine, mine).unwrap(), Policy::Allow);
        assert_eq!(policy(&store, mine, bob).unwrap(), Policy::Ask);
        for (scope, want, now) in [
            ("always", Policy::Allow, 1),
            ("refused", Policy::Refuse, 2),
            ("revoked", Policy::Ask, 3),
        ] {
            set_policy(&store, &me, bob, scope, "me", now).unwrap();
            assert_eq!(policy(&store, mine, bob).unwrap(), want, "{scope}");
        }
        // Another member's record does not touch bob's; the signature verifies.
        assert_eq!(policy(&store, mine, "carol").unwrap(), Policy::Ask);
        let rec: String = store
            .conn()
            .query_row(
                "SELECT record FROM approvals ORDER BY rowid DESC",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&rec).unwrap();
        assert!(crate::identity::verify(
            &me.key,
            &v,
            v["sig"].as_str().unwrap()
        ));
    }
}
