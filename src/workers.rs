//! Network workers, host side: what this computer offers and whom it lets start a worker.

use crate::config::{expand_home, Config};
use crate::holds::signed_record;
use crate::identity::Identity;
use crate::sessions::PROVIDERS;
use crate::store::Store;
use crate::Result;
use rusqlite::params;
use std::path::PathBuf;

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
