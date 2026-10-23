//! Whether another member's message may reach a session here: `people` (computer members'
//! `person_id`s), `cloud_of` (cloud members' `ws:<id>`), and `agents_send_without_asking` (my
//! own workspace's member). One `policy` row each; a sender's kind picks its row.

use crate::roster::{Member, MemberKind, Roster};
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
const CLOUD_OF: &str = "cloud_of";
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
            let by = by.unwrap_or_else(|| "local".into());
            let map = match scope.as_str() {
                SWITCH => {
                    p.agents_send_without_asking = value == "true";
                    p.set_by.insert(scope, by);
                    continue;
                }
                PEOPLE => &mut p.people,
                CLOUD_OF => &mut p.cloud_of,
                _ => continue,
            };
            if let Some(v) = Verdict::parse(&value) {
                map.insert(pid.clone(), v);
                p.set_by.insert(format!("{scope}:{pid}"), by);
            }
        }
        Ok(p)
    }
}

/// Whether `from` is the cloud member of my (member `me`'s) own workspace.
pub fn my_workspace(roster: &Roster, me: &str, from: &Member) -> bool {
    roster
        .members
        .get(me)
        .and_then(|m| roster.my_workspace_member(m))
        .is_some_and(|w| w.key == from.key)
}

/// `from`'s row, by its kind: my workspace's member is the switch, another cloud member
/// `cloud_of[pid]`, a computer `people[pid]`.
fn row<'a>(roster: &Roster, me: &str, from: &'a Member) -> (&'static str, &'a str) {
    if my_workspace(roster, me, from) {
        (SWITCH, "")
    } else if from.kind == MemberKind::Cloud {
        (CLOUD_OF, &from.person_id)
    } else {
        (PEOPLE, &from.person_id)
    }
}

/// The verdict on member `from`'s message to a session of member `me`: my own person accepts;
/// my workspace's member follows the switch; anyone else what its row says, else hold.
pub fn decide(store: &Store, roster: &Roster, me: &str, from: &Member) -> Result<Verdict> {
    if from.person_id == roster.person_of(me) {
        return Ok(Verdict::Accept);
    }
    let (scope, pid) = row(roster, me, from);
    let value: Option<String> = store
        .conn()
        .query_row(
            "SELECT value FROM policy WHERE scope = ?1 AND person_id = ?2",
            [scope, pid],
            |r| r.get(0),
        )
        .optional()?;
    Ok(match (scope, value.as_deref()) {
        (SWITCH, Some("true")) => Verdict::Accept,
        (SWITCH, _) => Verdict::Hold,
        (_, v) => v.and_then(Verdict::parse).unwrap_or(Verdict::Hold),
    })
}

/// Writes `v` for `from` in its row (the switch is on only for `accept`), set by `by`.
pub fn set(
    store: &Store,
    roster: &Roster,
    me: &str,
    from: &Member,
    v: Verdict,
    by: &str,
    now: i64,
) -> Result<()> {
    let (scope, pid) = row(roster, me, from);
    let value = match scope {
        SWITCH if v == Verdict::Accept => "true",
        SWITCH => "false",
        _ => v.as_str(),
    };
    put(store, scope, pid, value, by, now)
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

/// Writes `people[pid]` as `v`: a person no member of the roster is (yet).
pub fn set_person(store: &Store, pid: &str, v: Verdict, by: &str, now: i64) -> Result<()> {
    put(store, PEOPLE, pid, v.as_str(), by, now)
}

fn put(store: &Store, scope: &str, pid: &str, value: &str, by: &str, now: i64) -> Result<()> {
    store.conn().execute(
        "INSERT OR REPLACE INTO policy VALUES (?1, ?2, ?3, ?4, ?5)",
        params![scope, pid, value, by, now],
    )?;
    Ok(())
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

    fn roster(ms: &[&Member]) -> Roster {
        let mut r = Roster::default();
        for m in ms {
            r.members.insert(m.key.clone(), (*m).clone());
        }
        r
    }

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("state.db")).unwrap();
        (d, s)
    }

    #[test]
    fn defaults_accept_own_person_hold_others() {
        let (_d, s) = store();
        let me = member("me", "p-me", MemberKind::Computer, Some("w1"));
        let my_laptop = member("lap", "p-me", MemberKind::Computer, None);
        let aden = member("aden", "p-aden", MemberKind::Computer, None);
        let my_ws = member("ws1", "ws:w1", MemberKind::Cloud, None);
        let other_ws = member("ws2", "ws:w2", MemberKind::Cloud, None);
        let r = roster(&[&me, &my_laptop, &aden, &my_ws, &other_ws]);
        let d = |r: &Roster, from: &Member| decide(&s, r, "me", from).unwrap();
        assert_eq!(d(&r, &my_laptop), Verdict::Accept);
        assert_eq!(d(&r, &aden), Verdict::Hold);
        assert_eq!(d(&r, &my_ws), Verdict::Hold);
        assert_eq!(d(&r, &other_ws), Verdict::Hold);
        // Without a workspace of my own, no cloud member is mine.
        let no_ws = member("me", "p-me", MemberKind::Computer, None);
        let r2 = roster(&[&no_ws, &my_ws]);
        set_switch(&s, true, "local", 1).unwrap();
        assert_eq!(d(&r2, &my_ws), Verdict::Hold);
        assert_eq!(d(&r, &my_ws), Verdict::Accept);
        set(&s, &r, "me", &aden, Verdict::Refuse, "local", 1).unwrap();
        set(&s, &r, "me", &other_ws, Verdict::Accept, "local", 1).unwrap();
        assert_eq!(d(&r, &aden), Verdict::Refuse);
        assert_eq!(d(&r, &other_ws), Verdict::Accept);
        // Each kind reads its own table only.
        set_person(&s, "ws:w2", Verdict::Refuse, "local", 1).unwrap();
        assert_eq!(d(&r, &other_ws), Verdict::Accept);
        set_person(&s, "p-me", Verdict::Refuse, "local", 1).unwrap();
        assert_eq!(d(&r, &my_laptop), Verdict::Accept, "own person");
        // A computer whose person_id looks like a workspace is still a computer.
        let odd = member("odd", "ws:w1", MemberKind::Computer, None);
        let r3 = roster(&[&me, &odd]);
        set(&s, &r3, "me", &odd, Verdict::Refuse, "local", 1).unwrap();
        assert!(Policy::load(&s).unwrap().agents_send_without_asking);
        assert_eq!(d(&r3, &odd), Verdict::Refuse);
    }

    #[test]
    fn policy_round_trips_through_the_store() {
        let (_d, s) = store();
        let me = member("me", "p-me", MemberKind::Computer, Some("w1"));
        let p1 = member("p1", "p1", MemberKind::Computer, None);
        let w = member("w", "ws:w", MemberKind::Cloud, None);
        let mine = member("w1", "ws:w1", MemberKind::Cloud, None);
        let r = roster(&[&me, &p1, &w, &mine]);
        set(&s, &r, "me", &p1, Verdict::Accept, "local", 1).unwrap();
        set(&s, &r, "me", &w, Verdict::Hold, "bridge", 1).unwrap();
        set(&s, &r, "me", &mine, Verdict::Accept, "local", 1).unwrap();
        let p = Policy::load(&s).unwrap();
        assert_eq!(p.people["p1"], Verdict::Accept);
        assert_eq!(p.cloud_of["ws:w"], Verdict::Hold);
        assert!(!p.people.contains_key("ws:w"));
        assert!(p.agents_send_without_asking, "my workspace is the switch");
        assert!(!p.cloud_of.contains_key("ws:w1"));
        assert_eq!(p.set_by["cloud_of:ws:w"], "bridge");
        set(&s, &r, "me", &mine, Verdict::Hold, "local", 2).unwrap();
        assert!(!Policy::load(&s).unwrap().agents_send_without_asking);
    }
}
