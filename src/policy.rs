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

/// Whether member key `from` is the cloud member, in force, of my (member `me`'s) own
/// workspace (`Roster::my_workspace_member`), and, with `for_person`, that is my person: the
/// one check behind the switch, input grants and app sends.
pub fn my_workspace(roster: &Roster, me: &str, from: &str, for_person: Option<&str>) -> bool {
    roster.members.get(me).is_some_and(|m| {
        for_person.is_none_or(|p| p == m.person_id)
            && roster.my_workspace_member(m).is_some_and(|w| w.key == from)
    })
}

/// The inverse, as an instance asks it of a holder: member `key`, if it is a computer of person
/// `person` whose own workspace's member is me (`my_workspace` from its side).
pub fn workspace_computer<'a>(
    roster: &'a Roster,
    me: &str,
    key: &str,
    person: &str,
) -> Option<&'a Member> {
    (roster.members.get(key))
        .filter(|m| m.kind == MemberKind::Computer && my_workspace(roster, key, me, Some(person)))
}

/// `from`'s row, by its kind: my workspace's member is the switch, another cloud member
/// `cloud_of[pid]`, a computer `people[pid]`.
fn row<'a>(roster: &Roster, me: &str, from: &'a Member) -> (&'static str, &'a str) {
    if my_workspace(roster, me, &from.key, None) {
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

/// How long a session this workspace member wrote to may answer it.
const REPLY_S: u64 = 86_400;

/// The fixed rule on a workspace member (`mine`, an embedded instance) for `from`'s envelope
/// from session `from_addr` (ruling §4): a computer of its own workspace starts a
/// conversation; anyone else may only answer, within a day, a session one of its agents wrote
/// to and that took it (its sent mail, with a `stored`, `delivered` or `read` receipt, is the
/// record); everyone else is refused. It never holds.
pub fn cloud_inbound(
    store: &Store,
    roster: &Roster,
    mine: &Member,
    from: &Member,
    from_addr: &str,
    now: u64,
) -> Result<Verdict> {
    let own = workspace_computer(roster, &mine.key, &from.key, &from.person_id).is_some();
    // `<short>/` up to `<short>0` (`0` sorts just after `/`): a range, not a function of the
    // column.
    let (prefix, end) = (format!("{}/", mine.short), format!("{}0", mine.short));
    let since = now.saturating_sub(REPLY_S) as i64;
    let wrote = || -> Result<bool> {
        Ok(store
            .conn()
            .prepare(
                "SELECT 1 FROM messages m JOIN receipts r ON r.from_key = m.from_key \
                 AND r.request_id = m.request_id WHERE m.to_key = ?1 \
                 AND m.from_key >= ?2 AND m.from_key < ?3 AND m.sent_at >= ?4 \
                 AND r.state IN ('stored', 'delivered', 'read')",
            )?
            .exists(rusqlite::params![from_addr, prefix, end, since])?)
    };
    Ok(if own || wrote()? {
        Verdict::Accept
    } else {
        Verdict::Refuse
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

/// Writes `v` for a `pid` no member of the roster has (yet), in the table its kind will read:
/// a `ws:<id>` in `cloud_of`, any other id in `people`.
pub fn set_person(store: &Store, pid: &str, v: Verdict, by: &str, now: i64) -> Result<()> {
    let scope = if pid.starts_with("ws:") {
        CLOUD_OF
    } else {
        PEOPLE
    };
    put(store, scope, pid, v.as_str(), by, now)
}

/// Drops the rows of `pid` (`policy <id> forget`): an old row stored under a literal name.
pub fn forget(store: &Store, pid: &str) -> Result<()> {
    store.conn().execute(
        "DELETE FROM policy WHERE scope IN (?1, ?2) AND person_id = ?3",
        params![PEOPLE, CLOUD_OF, pid],
    )?;
    Ok(())
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
            by: None,
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
        // A workspace member in force names its workspace (`Roster::my_workspace_member`).
        let my_ws = member("ws1", "ws:w1", MemberKind::Cloud, Some("w1"));
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
        put(&s, PEOPLE, "ws:w2", "refuse", "local", 1).unwrap();
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
    fn an_unmatched_ws_id_is_a_cloud_of_entry() {
        let (_d, s) = store();
        set_person(&s, "ws:w3", Verdict::Refuse, "local", 1).unwrap();
        set_person(&s, "p-later", Verdict::Accept, "local", 1).unwrap();
        let p = Policy::load(&s).unwrap();
        assert_eq!(p.cloud_of["ws:w3"], Verdict::Refuse);
        assert!(!p.people.contains_key("ws:w3"));
        assert_eq!(p.people["p-later"], Verdict::Accept);
        // w3's member, admitted later, is refused, not held.
        let me = member("me", "p-me", MemberKind::Computer, Some("w1"));
        let w3 = member("w3", "ws:w3", MemberKind::Cloud, None);
        let r = roster(&[&me, &w3]);
        assert_eq!(decide(&s, &r, "me", &w3).unwrap(), Verdict::Refuse);
    }

    #[test]
    fn policy_round_trips_through_the_store() {
        let (_d, s) = store();
        let me = member("me", "p-me", MemberKind::Computer, Some("w1"));
        let p1 = member("p1", "p1", MemberKind::Computer, None);
        let w = member("w", "ws:w", MemberKind::Cloud, None);
        let mine = member("w1", "ws:w1", MemberKind::Cloud, Some("w1"));
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
