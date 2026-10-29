//! Network workers, host side: what this computer offers and whom it lets start a worker.

use crate::config::{expand_home, Config};
use crate::daemon::Daemon;
use crate::holds::{self, signed_record};
use crate::identity::Identity;
use crate::mail::{self, State};
use crate::sessions::{self, PROVIDERS};
use crate::store::{Message, Store, Worker};
use crate::Result;
use rusqlite::params;
use std::path::PathBuf;
use std::sync::Arc;

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

/// Starts the worker for assignment `assign_id`. Task 4 spawns the harness; for now it only
/// records the worker as `starting`.
pub fn start(d: &Arc<Daemon>, assign_id: &str) -> Result<()> {
    let team = d.team.lock().unwrap();
    let store = d.store.lock().unwrap();
    let Some(a) = store.message(assign_id)? else {
        return Err(format!("no assignment {assign_id}").into());
    };
    let provider = (sessions::split_addr(&a.to_key))
        .and_then(|(_, rest)| rest.strip_suffix(":new"))
        .unwrap_or_default();
    let now = crate::daemon::ipc::now() as i64;
    store.insert_worker(&Worker {
        id: 0,
        assignment: a.id.clone(),
        session: a.to_key.clone(),
        provider: provider.into(),
        folder: folder(&team.config).join(&a.request_id),
        pid: None,
        started_at: now,
        ended_at: None,
        state: "starting".into(),
        by: None,
    })?;
    eprintln!("worker for {assign_id}: starting");
    Ok(())
}

/// The asker `by` cancelled assignment `assign_id` addressed to a fresh worker: an open start
/// hold is declined and its dialog withdrawn; a worker row ends `cancelled`. Task 4 also stops
/// the process.
pub fn cancel(d: &Arc<Daemon>, assign_id: &str, by: &str) -> Result<()> {
    let store = d.store.lock().unwrap();
    let now = crate::daemon::ipc::now() as i64;
    let holds_open: Vec<i64> = {
        let mut stmt = store
            .conn()
            .prepare("SELECT id FROM holds WHERE message_id = ?1 AND state = 'open'")?;
        let rows = stmt.query_map([assign_id], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for id in holds_open {
        store
            .conn()
            .execute("UPDATE holds SET state = 'declined' WHERE id = ?1", [id])?;
        d.dialogs.withdraw(&format!("H{id}"));
    }
    if let Some(w) = store
        .worker_by_assignment(assign_id)?
        .filter(|w| w.ended_at.is_none())
    {
        store.end_worker(w.id, "cancelled", Some(by), now)?;
    }
    eprintln!("worker for {assign_id}: cancelled");
    Ok(())
}

/// A person asked to stop the worker `session` (the dialog's Stop worker). Task 4 fills it.
pub fn stop_requested(_d: &std::sync::Arc<crate::daemon::Daemon>, session: &str, by: &str) {
    eprintln!("worker stop requested session={session} by={by}");
}

#[cfg(test)]
mod tests {
    use super::*;

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
