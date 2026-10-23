//! Whether another member's message may reach a session here: `people` (computer members'
//! `person_id`s), `cloud_of` (a workspace's `ws:<id>`, for cloud members), and
//! `agents_send_without_asking` (my own workspace's member). One `policy` row each.

use crate::roster::{Member, MemberKind};
use crate::store::Store;
use crate::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Accept,
    Hold,
    Refuse,
}

/// The `policy` row that carries the switch (its `person_id` is empty).
pub const SWITCH: &str = "agents_send_without_asking";

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Policy {
    pub people: BTreeMap<String, Verdict>,
    pub cloud_of: BTreeMap<String, Verdict>,
    pub agents_send_without_asking: bool,
    /// Who set each row (`local` or `bridge`), keyed `people:<pid>`, `cloud_of:<pid>` or
    /// `agents_send_without_asking`.
    pub set_by: BTreeMap<String, String>,
}

impl Policy {
    pub fn load(store: &Store) -> Result<Policy> {
        let mut p = Policy::default();
        let mut stmt = store
            .conn()
            .prepare("SELECT scope, person_id, value, set_by FROM policy")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (scope, pid, value, by) = row?;
            let key = match scope.as_str() {
                SWITCH => {
                    p.agents_send_without_asking = value == "true";
                    scope
                }
                "people" | "cloud_of" => {
                    let v = serde_json::from_value(value.into())?;
                    let map = if scope == "people" {
                        &mut p.people
                    } else {
                        &mut p.cloud_of
                    };
                    map.insert(pid.clone(), v);
                    format!("{scope}:{pid}")
                }
                _ => continue,
            };
            p.set_by.insert(key, by.unwrap_or_else(|| "local".into()));
        }
        Ok(p)
    }
}

/// Writes `v` for person `pid`, in the one place its kind reads: my own workspace's
/// `ws:<my_workspace>` is the switch (on only for `accept`), another workspace's `ws:<id>` is
/// `cloud_of`, anyone else `people`.
pub fn set(
    store: &Store,
    my_workspace: Option<&str>,
    pid: &str,
    v: Verdict,
    by: &str,
    now: i64,
) -> Result<()> {
    if my_workspace.is_some_and(|w| pid.strip_prefix("ws:") == Some(w)) {
        return set_switch(store, v == Verdict::Accept, by, now);
    }
    let scope = if pid.starts_with("ws:") {
        "cloud_of"
    } else {
        "people"
    };
    let value = serde_json::to_value(v)?;
    put(
        store,
        scope,
        pid,
        value.as_str().unwrap_or_default(),
        by,
        now,
    )
}

/// Writes `agents_send_without_asking`.
pub fn set_switch(store: &Store, on: bool, by: &str, now: i64) -> Result<()> {
    put(
        store,
        SWITCH,
        "",
        if on { "true" } else { "false" },
        by,
        now,
    )
}

fn put(store: &Store, scope: &str, pid: &str, value: &str, by: &str, now: i64) -> Result<()> {
    store.conn().execute(
        "INSERT OR REPLACE INTO policy VALUES (?1, ?2, ?3, ?4, ?5)",
        params![scope, pid, value, by, now],
    )?;
    Ok(())
}

/// Whether `from` is my own workspace's member: a cloud member whose `person_id` is
/// `ws:<my workspace>`.
pub fn is_my_workspace(me: &Member, from: &Member) -> bool {
    from.kind == MemberKind::Cloud
        && me
            .workspace
            .as_ref()
            .is_some_and(|w| from.person_id == format!("ws:{w}"))
}

/// The verdict on member `from`'s message to a session of `me`: my own person accepts; my
/// workspace's member follows the switch; another cloud member what `cloud_of` says for its
/// `person_id`, a computer what `people` says; else hold.
pub fn decide(p: &Policy, me: &Member, from: &Member) -> Verdict {
    if from.person_id == me.person_id {
        return Verdict::Accept;
    }
    if is_my_workspace(me, from) {
        return match p.agents_send_without_asking {
            true => Verdict::Accept,
            false => Verdict::Hold,
        };
    }
    let table = match from.kind {
        MemberKind::Cloud => &p.cloud_of,
        MemberKind::Computer => &p.people,
    };
    table.get(&from.person_id).copied().unwrap_or(Verdict::Hold)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roster::Role;

    fn member(key: &str, person: &str, kind: MemberKind, ws: Option<&str>) -> Member {
        Member {
            key: key.into(),
            short: key.into(),
            name: key.into(),
            computer: "c".into(),
            person_id: person.into(),
            role: Role::Member,
            kind,
            admitted_at: 0,
            proof: None,
            workspace: ws.map(Into::into),
        }
    }

    #[test]
    fn defaults_accept_own_person_hold_others() {
        let me = member("me", "p-me", MemberKind::Computer, Some("w1"));
        let my_laptop = member("lap", "p-me", MemberKind::Computer, None);
        let aden = member("aden", "p-aden", MemberKind::Computer, None);
        let my_ws = member("ws1", "ws:w1", MemberKind::Cloud, None);
        let other_ws = member("ws2", "ws:w2", MemberKind::Cloud, None);
        let mut p = Policy::default();
        assert_eq!(decide(&p, &me, &my_laptop), Verdict::Accept);
        assert_eq!(decide(&p, &me, &aden), Verdict::Hold);
        assert_eq!(decide(&p, &me, &my_ws), Verdict::Hold);
        assert_eq!(decide(&p, &me, &other_ws), Verdict::Hold);
        // Without a workspace of my own, no cloud member is mine.
        let no_ws = member("me", "p-me", MemberKind::Computer, None);
        p.agents_send_without_asking = true;
        assert_eq!(decide(&p, &no_ws, &my_ws), Verdict::Hold);
        assert_eq!(decide(&p, &me, &my_ws), Verdict::Accept);
        p.people.insert("p-aden".into(), Verdict::Refuse);
        p.cloud_of.insert("ws:w2".into(), Verdict::Accept);
        assert_eq!(decide(&p, &me, &aden), Verdict::Refuse);
        assert_eq!(decide(&p, &me, &other_ws), Verdict::Accept);
        // Each kind reads its own table only.
        p.people.insert("ws:w2".into(), Verdict::Refuse);
        assert_eq!(decide(&p, &me, &other_ws), Verdict::Accept);
        p.people.insert("p-me".into(), Verdict::Refuse);
        assert_eq!(decide(&p, &me, &my_laptop), Verdict::Accept, "own person");
    }

    #[test]
    fn policy_round_trips_through_the_store() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("state.db")).unwrap();
        let mine = Some("w1");
        set(&s, mine, "p1", Verdict::Accept, "local", 1).unwrap();
        set(&s, mine, "ws:w", Verdict::Hold, "bridge", 1).unwrap();
        set(&s, mine, "ws:w1", Verdict::Accept, "local", 1).unwrap();
        let p = Policy::load(&s).unwrap();
        assert_eq!(p.people["p1"], Verdict::Accept);
        assert_eq!(p.cloud_of["ws:w"], Verdict::Hold);
        assert!(!p.people.contains_key("ws:w"));
        assert!(p.agents_send_without_asking, "my workspace is the switch");
        assert!(!p.cloud_of.contains_key("ws:w1"));
        assert_eq!(p.set_by["cloud_of:ws:w"], "bridge");
        set(&s, mine, "ws:w1", Verdict::Hold, "local", 2).unwrap();
        assert!(!Policy::load(&s).unwrap().agents_send_without_asking);
    }
}
