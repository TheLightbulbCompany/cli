//! Whether another member's message may reach a session here: `people`, one `policy` row per
//! member (by its `person_id`, its own key), else held. On a workspace's instance its
//! `members` decide instead, by sign-in proof (`mail::receive`, `proof::admitted`).

use crate::roster::Member;
use crate::store::Store;
use crate::Result;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Accept,
    Hold,
    Refuse,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Accept => "accept",
            Verdict::Hold => "hold",
            Verdict::Refuse => "refuse",
        }
    }

    pub fn parse(s: &str) -> Option<Verdict> {
        [Verdict::Accept, Verdict::Hold, Verdict::Refuse]
            .into_iter()
            .find(|v| v.as_str() == s)
    }
}

const PEOPLE: &str = "people";

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Policy {
    pub people: BTreeMap<String, Verdict>,
    /// Who set each row (`local` or `bridge`), keyed `people:<pid>`.
    pub set_by: BTreeMap<String, String>,
}

impl Policy {
    pub fn load(store: &Store) -> Result<Policy> {
        let mut p = Policy::default();
        let mut stmt = store
            .conn()
            .prepare("SELECT person_id, value, set_by FROM policy WHERE scope = ?1")?;
        let rows = stmt.query_map([PEOPLE], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (pid, value, by) = row?;
            if let Some(v) = Verdict::parse(&value) {
                p.people.insert(pid.clone(), v);
                p.set_by.insert(
                    format!("{PEOPLE}:{pid}"),
                    by.unwrap_or_else(|| "local".into()),
                );
            }
        }
        Ok(p)
    }
}

/// The verdict on member `from`'s message to a session of member `me`: my own computer
/// accepts; anyone else what its row says, else hold.
pub fn decide(store: &Store, me: &str, from: &Member) -> Result<Verdict> {
    if from.key == me {
        return Ok(Verdict::Accept);
    }
    let value: Option<String> = store
        .conn()
        .query_row(
            "SELECT value FROM policy WHERE scope = ?1 AND person_id = ?2",
            [PEOPLE, &from.person_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(value
        .as_deref()
        .and_then(Verdict::parse)
        .unwrap_or(Verdict::Hold))
}

/// Writes `v` for `from`, set by `by`.
pub fn set(store: &Store, from: &Member, v: Verdict, by: &str, now: i64) -> Result<()> {
    set_person(store, &from.person_id, v, by, now)
}

/// Writes `v` for person `pid`.
pub fn set_person(store: &Store, pid: &str, v: Verdict, by: &str, now: i64) -> Result<()> {
    store.conn().execute(
        "INSERT OR REPLACE INTO policy VALUES (?1, ?2, ?3, ?4, ?5)",
        params![PEOPLE, pid, v.as_str(), by, now],
    )?;
    Ok(())
}

/// Drops the rows of `pid` (`policy <id> forget`): an old row stored under a literal name.
pub fn forget(store: &Store, pid: &str) -> Result<()> {
    store
        .conn()
        .execute("DELETE FROM policy WHERE person_id = ?1", params![pid])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roster::Role;

    fn member(key: &str) -> Member {
        Member {
            key: key.into(),
            short: key.into(),
            name: key.into(),
            computer: "c".into(),
            person_id: key.into(),
            role: Role::Member,
            admitted_at: 0,
            by: None,
        }
    }

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("state.db")).unwrap();
        (d, s)
    }

    #[test]
    fn my_own_computer_accepts_others_hold_until_a_row_says_otherwise() {
        let (_d, s) = store();
        let (me, aden) = (member("me"), member("aden"));
        let d = |from: &Member| decide(&s, "me", from).unwrap();
        assert_eq!(d(&me), Verdict::Accept);
        assert_eq!(d(&aden), Verdict::Hold);
        set(&s, &aden, Verdict::Refuse, "local", 1).unwrap();
        assert_eq!(d(&aden), Verdict::Refuse);
        set_person(&s, "me", Verdict::Refuse, "local", 1).unwrap();
        assert_eq!(d(&me), Verdict::Accept, "own computer");
        set(&s, &aden, Verdict::Accept, "bridge", 2).unwrap();
        let p = Policy::load(&s).unwrap();
        assert_eq!(p.people["aden"], Verdict::Accept);
        assert_eq!(p.set_by["people:aden"], "bridge");
        forget(&s, "aden").unwrap();
        assert_eq!(d(&aden), Verdict::Hold);
    }
}
