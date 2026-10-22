//! The roster: the team's membership log. A hash-linked DAG of signed entries with one root,
//! replayed deterministically, so every member computes the same team from the same set of
//! entries whatever order they arrived in. Pure: no network, no IPC, no logging.

use crate::identity::{unsigned, verify, Identity};
use crate::store::Store;
use crate::Result;
use data_encoding::HEXLOWER;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// How far ahead of our clock an entry may be stamped.
const MAX_SKEW_S: i64 = 86_400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Admit,
    Merge,
    Proof,
    Remove,
    Admin,
    Transfer,
    Leave,
    Link,
    Meta,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub v: u32,
    pub parents: Vec<String>,
    pub kind: Kind,
    pub body: Value,
    pub signer: String,
    pub at: i64,
    pub sig: String,
}

impl Entry {
    pub fn signed(id: &Identity, parents: Vec<String>, kind: Kind, body: Value, at: i64) -> Entry {
        let mut e = Entry {
            v: 1,
            parents,
            kind,
            body,
            signer: id.key.clone(),
            at,
            sig: String::new(),
        };
        e.sig = id.sign(&e.value());
        e
    }

    /// Lowercase hex sha256 of `canonical(entry without sig)`.
    pub fn hash(&self) -> String {
        HEXLOWER.encode(&Sha256::digest(unsigned(&self.value())))
    }

    fn value(&self) -> Value {
        serde_json::to_value(self).expect("an entry serializes")
    }

    fn text(&self, field: &str) -> &str {
        self.body[field].as_str().unwrap_or_default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Member,
    Admin,
    Founder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberKind {
    Computer,
    Cloud,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    pub key: String,
    pub short: String,
    pub name: String,
    pub computer: String,
    pub person_id: String,
    pub role: Role,
    pub kind: MemberKind,
    pub admitted_at: i64,
    pub proof: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub ticket_id: String,
    pub secret: String,
    pub mode: String,
    pub once: bool,
    pub expires_at: i64,
    pub signer: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Roster {
    pub team_id: String,
    pub team_name: String,
    pub members: BTreeMap<String, Member>,
    pub heads: Vec<String>,
    pub links: BTreeMap<String, Link>,
    pub relay: Option<String>,
    parents: BTreeMap<String, Vec<String>>,
}

impl Roster {
    pub fn by_short(&self, short: &str) -> Option<&Member> {
        self.members.values().find(|m| m.short == short)
    }

    /// Every entry `hash` descends from, itself excluded.
    pub fn ancestors(&self, hash: &str) -> HashSet<String> {
        let mut seen = HashSet::new();
        let mut todo: Vec<&str> = vec![hash];
        while let Some(h) = todo.pop() {
            for p in self.parents.get(h).into_iter().flatten() {
                if seen.insert(p.clone()) {
                    todo.push(p);
                }
            }
        }
        seen
    }
}

/// The team as of `entries`, the same whatever their order. Kahn topological order (ready
/// `remove` entries first, then lowest hash); an entry is applied only if its signer is active
/// and authorised there and no `remove` of its signer is concurrent with it. A `remove` is
/// judged on its own causal past, so concurrent removals all win.
pub fn replay(entries: &[Entry]) -> Roster {
    let by_hash: BTreeMap<String, &Entry> = entries.iter().map(|e| (e.hash(), e)).collect();
    // The heads are the entries no entry names as a parent.
    let named: HashSet<&str> = by_hash
        .values()
        .flat_map(|e| e.parents.iter().map(String::as_str))
        .collect();
    let graph = Roster {
        heads: by_hash
            .keys()
            .filter(|h| !named.contains(h.as_str()))
            .cloned()
            .collect(),
        parents: by_hash
            .iter()
            .map(|(h, e)| (h.clone(), e.parents.clone()))
            .collect(),
        ..Roster::default()
    };
    // ponytail: every ancestor set up front, O(n^2); index the DAG if rosters grow large.
    let anc: BTreeMap<&str, HashSet<String>> = by_hash
        .keys()
        .map(|h| (h.as_str(), graph.ancestors(h)))
        .collect();
    // A removal deeper in its branch than a concurrent entry is replayed after it, so replay
    // again knowing every removal any pass applied: each drops its target's concurrent entries
    // ahead of its own position. `known` only grows, so this settles within one pass per
    // removal. ponytail: the cap (#removes + 1) is a safety only.
    let cap = by_hash.values().filter(|e| e.kind == Kind::Remove).count() + 1;
    let mut known = BTreeSet::new();
    let (mut r, mut applied) = pass(&by_hash, &anc, &known, graph.clone());
    for _ in 1..cap {
        if applied.is_subset(&known) {
            break;
        }
        known.extend(applied.iter().cloned());
        (r, applied) = pass(&by_hash, &anc, &known, graph.clone());
    }
    // A known removal whose signer's admission no longer survives is stale: replay once more
    // knowing only the removals that still apply, and keep that if it agrees with itself.
    if applied != known {
        let (exact, again) = pass(&by_hash, &anc, &applied, graph);
        if again == applied {
            return exact;
        }
    }
    r
}

/// One replay in which the `known` removals drop their targets' concurrent entries ahead of
/// their own position; returns the roster and the removals it applied.
fn pass(
    by_hash: &BTreeMap<String, &Entry>,
    anc: &BTreeMap<&str, HashSet<String>>,
    known: &BTreeSet<String>,
    mut r: Roster,
) -> (Roster, BTreeSet<String>) {
    let mut waiting: BTreeMap<&str, usize> = BTreeMap::new();
    let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (h, e) in by_hash {
        waiting.insert(h, e.parents.len());
        for p in &e.parents {
            children.entry(p.as_str()).or_default().push(h);
        }
    }
    let ready_key = |h: &str| (by_hash[h].kind != Kind::Remove, h.to_string());
    let mut ready: BTreeSet<(bool, String)> = waiting
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(h, _)| ready_key(h))
        .collect();
    // `removals` = `known` plus those applied so far; `gone` = removed members and the
    // removal that took each.
    let mut removals = known.clone();
    let mut applied = BTreeSet::new();
    let mut gone: BTreeMap<String, (Member, String)> = BTreeMap::new();
    // Links by `ticket_id`, each with the hash of its entry.
    let mut links: BTreeMap<String, (String, Link)> = BTreeMap::new();
    let mut used_once: BTreeSet<String> = BTreeSet::new();
    let mut admins: Vec<String> = Vec::new();
    while let Some((_, h)) = ready.pop_first() {
        for c in children.get(h.as_str()).into_iter().flatten() {
            let n = waiting.get_mut(c).expect("a child is an entry");
            *n -= 1;
            if *n == 0 {
                ready.insert(ready_key(c));
            }
        }
        let e = by_hash[&h];
        if r.team_id.is_empty() {
            if e.parents.is_empty() && is_genesis(e) {
                r.team_id = h.clone();
                r.team_name = e.text("team_name").to_string();
                r.members
                    .insert(e.signer.clone(), new_member(e, Role::Founder));
            }
            continue;
        }
        // Exactly one root.
        if e.parents.is_empty() {
            continue;
        }
        let target = e.text("member").to_string();
        let signer = match r.members.get(&e.signer) {
            Some(m) => m.clone(),
            // A removal stands on its own causal past: a concurrent removal of its signer
            // does not stop it (`gone` keeps the signer's role from before that removal).
            None if e.kind == Kind::Remove => match gone.get(&e.signer) {
                Some((m, rh)) if !anc[h.as_str()].contains(rh) => m.clone(),
                _ => continue,
            },
            None => continue,
        };
        // Any other entry concurrent with a removal of its signer is dropped.
        if e.kind != Kind::Remove
            && removals.iter().any(|rh| {
                by_hash[rh].text("member") == e.signer
                    && !anc[rh.as_str()].contains(&h)
                    && !anc[h.as_str()].contains(rh)
            })
        {
            continue;
        }
        let elevated = signer.role != Role::Member;
        let computer = signer.kind == MemberKind::Computer;
        let founder = signer.role == Role::Founder;
        match e.kind {
            // A cloud (bridge) member never admits.
            Kind::Admit if computer => {
                let link = links.get(e.text("ticket_id"));
                if link.is_some_and(|(lh, l)| l.once && used_once.contains(lh)) {
                    continue;
                }
                let via_link = || {
                    link.is_some_and(|(lh, l)| {
                        l.signer == e.signer
                            && anc[h.as_str()].contains(lh)
                            && removals.iter().all(|rh| {
                                !anc[rh.as_str()].contains(lh) || anc[rh.as_str()].contains(&h)
                            })
                    })
                };
                let Some(prefix) = target.get(..12) else {
                    continue;
                };
                let name = e.text("name").to_lowercase();
                let collides = r
                    .members
                    .values()
                    .any(|m| m.short == prefix || m.name.to_lowercase() == name);
                let role_ok = matches!(
                    e.body.get("role").and_then(Value::as_str),
                    None | Some("member")
                );
                if (elevated || via_link()) && !collides && role_ok && own_person(e) {
                    r.members.insert(target, new_member(e, Role::Member));
                    if let Some((lh, _)) = link.filter(|(_, l)| l.once) {
                        used_once.insert(lh.clone());
                    }
                }
            }
            Kind::Merge => {
                if let Some(m) = r.members.get_mut(&target) {
                    let pid = e.text("person_id");
                    if target == e.signer && m.person_id == m.key && !pid.is_empty() {
                        m.person_id = pid.to_string();
                    }
                }
            }
            Kind::Proof => {
                if target == e.signer {
                    r.members
                        .get_mut(&target)
                        .expect("the signer is active")
                        .proof = Some(e.text("proof").to_string());
                }
            }
            Kind::Remove => {
                let Some(t) = r.members.get(&target) else {
                    continue;
                };
                // `unlink`: a member removing its own person's cloud member.
                // ponytail: any cloud member of the person; Task 13 narrows it to a bridge proof.
                let unlink = t.kind == MemberKind::Cloud && t.person_id == signer.person_id;
                if (elevated || unlink) && take(&mut r, &mut gone, &target, &h) {
                    removals.insert(h.clone());
                    applied.insert(h.clone());
                }
            }
            Kind::Admin if founder => {
                if let Some(t) = r.members.get_mut(&target) {
                    if t.role == Role::Member {
                        t.role = Role::Admin;
                        admins.push(target);
                    }
                }
            }
            Kind::Transfer if founder && target != e.signer => {
                if let Some(t) = r.members.get_mut(&target) {
                    t.role = Role::Founder;
                    r.members.get_mut(&e.signer).expect("active").role = Role::Admin;
                    admins.push(e.signer.clone());
                }
            }
            Kind::Leave => {
                if target == e.signer {
                    r.members.remove(&target);
                    if founder {
                        let oldest = admins
                            .iter()
                            .find(|a| r.members.get(*a).is_some_and(|m| m.role == Role::Admin));
                        if let Some(a) = oldest.cloned() {
                            r.members.get_mut(&a).expect("active").role = Role::Founder;
                        }
                    }
                }
            }
            // Only a computer member mints links, and a `ticket_id` names one link ever.
            Kind::Link if computer && !e.text("ticket_id").is_empty() => {
                links
                    .entry(e.text("ticket_id").to_string())
                    .or_insert_with(|| {
                        let link = Link {
                            ticket_id: e.text("ticket_id").to_string(),
                            secret: e.text("secret").to_string(),
                            mode: e.text("mode").to_string(),
                            once: e.body["once"].as_bool().unwrap_or(false),
                            expires_at: e.body["expires_at"].as_i64().unwrap_or(0),
                            signer: e.signer.clone(),
                        };
                        (h.clone(), link)
                    });
            }
            Kind::Meta => {
                if elevated {
                    if let Some(n) = e.body["team_name"].as_str() {
                        r.team_name = n.to_string();
                    }
                    if let Some(relay) = e.body["relay"].as_str() {
                        r.relay = Some(relay.to_string());
                    }
                }
            }
            Kind::Admit | Kind::Link | Kind::Admin | Kind::Transfer => {}
        }
    }
    // Live links: unused, and no removal descends from them.
    for (lh, l) in links.into_values() {
        if !used_once.contains(&lh) && !removals.iter().any(|rh| anc[rh.as_str()].contains(&lh)) {
            r.links.insert(l.ticket_id.clone(), l);
        }
    }
    (r, applied)
}

/// Removes `target` (never the founder) by removal `h`; whether it was removed.
fn take(
    r: &mut Roster,
    gone: &mut BTreeMap<String, (Member, String)>,
    target: &str,
    h: &str,
) -> bool {
    match r.members.get(target) {
        Some(m) if m.role != Role::Founder => {
            let m = r.members.remove(target).expect("present");
            gone.insert(target.to_string(), (m, h.to_string()));
            true
        }
        _ => false,
    }
}

/// Without a proof, an `admit` may only name the member's own key as its `person_id`; Task 13's
/// verified proof is the one way to claim another person's id.
fn own_person(e: &Entry) -> bool {
    e.body["proof"].is_string() || e.text("person_id") == e.text("member")
}

fn is_genesis(e: &Entry) -> bool {
    e.kind == Kind::Admit
        && own_person(e)
        && e.text("role") == "founder"
        && e.text("member") == e.signer
        && e.signer.get(..12).is_some()
}

fn new_member(e: &Entry, role: Role) -> Member {
    let key = e.text("member").to_string();
    let person_id = match e.text("person_id") {
        "" => key.clone(),
        p => p.to_string(),
    };
    Member {
        short: key.get(..12).unwrap_or_default().to_string(),
        name: e.text("name").to_string(),
        computer: e.text("computer").to_string(),
        person_id,
        role,
        kind: match e.text("kind") {
            "cloud" => MemberKind::Cloud,
            _ => MemberKind::Computer,
        },
        admitted_at: e.at,
        proof: e.body["proof"].as_str().map(str::to_string),
        key,
    }
}

/// Every stored entry.
pub fn entries(store: &Store) -> Result<Vec<Entry>> {
    let mut stmt = store
        .conn()
        .prepare("SELECT entry FROM roster ORDER BY hash")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(serde_json::from_str(&row?)?);
    }
    Ok(out)
}

/// Stores `entry` once it verifies: `Ok(true)` when stored, `Ok(false)` when already held.
/// Refuses a bad signature, a stamp more than a day ahead of `now`, an unknown parent and a
/// second root.
pub fn accept(store: &Store, entry: &Entry, now: i64) -> Result<bool> {
    if !verify(&entry.signer, &entry.value(), &entry.sig) {
        return Err("roster entry has a bad signature".into());
    }
    if entry.at > now + MAX_SKEW_S {
        return Err("roster entry is stamped in the future".into());
    }
    let hash = entry.hash();
    let conn = store.conn();
    let mut held = conn.prepare("SELECT 1 FROM roster WHERE hash = ?1")?;
    if held.exists([&hash])? {
        return Ok(false);
    }
    for p in &entry.parents {
        if !held.exists([p])? {
            return Err("roster entry names an unknown parent".into());
        }
    }
    if entry.parents.is_empty()
        && conn
            .prepare("SELECT 1 FROM roster WHERE json_array_length(entry, '$.parents') = 0")?
            .exists([])?
    {
        return Err("the roster already has a root".into());
    }
    // OR IGNORE: a concurrent accept of the same entry is `Ok(false)`, not an error.
    let n = conn.execute(
        "INSERT OR IGNORE INTO roster (hash, entry, received_at) VALUES (?1, ?2, ?3)",
        params![hash, serde_json::to_string(entry)?, now],
    )?;
    Ok(n == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn id() -> Identity {
        let dir = tempfile::tempdir().unwrap();
        Identity::load_or_create(dir.path()).unwrap()
    }

    fn op(by: &Identity, parent: &Entry, kind: Kind, body: Value) -> Entry {
        Entry::signed(by, vec![parent.hash()], kind, body, 10)
    }

    /// An `admit` body: `who`, a computer member of their own person, with no ticket.
    fn admit_body(who: &str, name: &str) -> Value {
        json!({"member": who, "name": name, "computer": "mac", "person_id": who,
               "kind": "computer", "role": "member", "ticket_id": null})
    }

    fn genesis(f: &Identity) -> Entry {
        let mut b = admit_body(&f.key, "founder");
        b["role"] = json!("founder");
        b["team_name"] = json!("acme");
        Entry::signed(f, vec![], Kind::Admit, b, 1)
    }

    fn admit_via(by: &Identity, parent: &Entry, who: &str, name: &str, ticket: &str) -> Entry {
        let mut b = admit_body(who, name);
        b["ticket_id"] = json!(ticket);
        op(by, parent, Kind::Admit, b)
    }

    fn admit(by: &Identity, parent: &Entry, who: &str, name: &str) -> Entry {
        admit_via(by, parent, who, name, "")
    }

    fn member(by: &Identity, parent: &Entry, kind: Kind, who: &str) -> Entry {
        op(by, parent, kind, json!({ "member": who }))
    }

    fn link_with(by: &Identity, parent: &Entry, ticket: &str, once: bool) -> Entry {
        op(
            by,
            parent,
            Kind::Link,
            json!({"ticket_id": ticket, "secret": "s", "mode": "ask", "once": once, "expires_at": 99}),
        )
    }

    /// X admitted, then made an admin, so X's own admissions are authorised.
    fn admit_admin(f: &Identity, parent: &Entry, x: &Identity) -> [Entry; 2] {
        let ax = admit(f, parent, &x.key, "x");
        let px = member(f, &ax, Kind::Admin, &x.key);
        [ax, px]
    }

    /// Same parent: X admits C | founder removes X.
    fn fork() -> (Vec<Entry>, [String; 2]) {
        let (f, x, c) = (id(), id(), id());
        let g = genesis(&f);
        let [ax0, ax] = admit_admin(&f, &g, &x);
        let xc = admit(&x, &ax, &c.key, "c");
        let rx = member(&f, &ax, Kind::Remove, &x.key);
        (vec![g, ax0, ax, xc, rx], [x.key, c.key])
    }

    /// Fork at F: X admits C, then X admits D | founder removes X.
    fn deep_fork() -> (Vec<Entry>, [String; 3]) {
        let (f, x, c, d) = (id(), id(), id(), id());
        let g = genesis(&f);
        let [ax0, ax] = admit_admin(&f, &g, &x);
        let xc = admit(&x, &ax, &c.key, "c");
        let xd = admit(&x, &xc, &d.key, "d");
        let rx = member(&f, &ax, Kind::Remove, &x.key);
        (vec![g, ax0, ax, xc, xd, rx], [x.key, c.key, d.key])
    }

    /// `op` re-stamped until its hash sorts after `floor`, so Kahn order is known.
    fn op_after(by: &Identity, parent: &Entry, kind: Kind, body: Value, floor: &Entry) -> Entry {
        (10..)
            .map(|at| Entry::signed(by, vec![parent.hash()], kind, body.clone(), at))
            .find(|e| e.hash() > floor.hash())
            .unwrap()
    }

    #[test]
    fn genesis_makes_a_founder_and_names_the_team() {
        let f = id();
        let g = genesis(&f);
        let second = Entry::signed(&f, vec![], Kind::Meta, json!({"team_name": "other"}), 2);
        let r = replay(&[g.clone(), second]);
        assert_eq!(r.team_id, g.hash());
        assert_eq!(r.team_name, "acme");
        assert_eq!(r.members[&f.key].role, Role::Founder);
        assert_eq!(r.by_short(&f.short).unwrap().key, f.key);
    }

    #[test]
    fn non_admin_admit_and_remove_are_dropped() {
        let (f, x, b, c) = (id(), id(), id(), id());
        let g = genesis(&f);
        let ax = admit(&f, &g, &x.key, "x");
        let ab = admit(&f, &ax, &b.key, "b");
        let xc = admit(&x, &ab, &c.key, "c");
        let xb = member(&x, &xc, Kind::Remove, &b.key);
        let r = replay(&[g, ax, ab, xc, xb]);
        assert!(r.members.contains_key(&b.key));
        assert!(!r.members.contains_key(&c.key));
        assert_eq!(r.members.len(), 3);
    }

    #[test]
    fn fork_removal_wins_and_drops_admissions_by_removed_signer() {
        let (entries, [x, c]) = fork();
        let r = replay(&entries);
        assert!(!r.members.contains_key(&x));
        assert!(!r.members.contains_key(&c));
        assert_eq!(r.members.len(), 1);
        assert_eq!(r.heads.len(), 2);
    }

    #[test]
    fn deep_fork_removal_wins() {
        let (entries, [x, c, d]) = deep_fork();
        let r = replay(&entries);
        for k in [x, c, d] {
            assert!(!r.members.contains_key(&k));
        }
        assert_eq!(r.members.len(), 1);
    }

    #[test]
    fn removal_below_other_entries_still_wins() {
        // Branch 2 reaches the removal only after two meta entries that sort after `xc`, so
        // the first pass replays X's admission of C before the removal: only the fix-point
        // drops it.
        let (f, x, c) = (id(), id(), id());
        let g = genesis(&f);
        let [ax0, ax] = admit_admin(&f, &g, &x);
        let xc = admit(&x, &ax, &c.key, "c");
        let m1 = op_after(&f, &ax, Kind::Meta, json!({"relay": "r1"}), &xc);
        let m2 = op_after(&f, &m1, Kind::Meta, json!({"team_name": "acme2"}), &xc);
        let rx = member(&f, &m2, Kind::Remove, &x.key);
        let r = replay(&[g, ax0, ax, xc, m1, m2, rx]);
        assert_eq!(r.members.len(), 1);
        assert_eq!(r.team_name, "acme2");
        assert_eq!(r.relay.as_deref(), Some("r1"));
    }

    #[test]
    fn removal_cycle_converges_and_is_stable() {
        // Admins A, B, C concurrently remove B, C, A: removal wins, so all three go, and
        // unrelated entries that change the replay order change nothing.
        let (f, a, b, c) = (id(), id(), id(), id());
        let g = genesis(&f);
        let mut log = vec![g.clone()];
        for (who, name) in [(&a, "a"), (&b, "b"), (&c, "c")] {
            let e = admit(&f, log.last().unwrap(), &who.key, name);
            log.push(e);
        }
        for who in [&a, &b, &c] {
            let e = member(&f, log.last().unwrap(), Kind::Admin, &who.key);
            log.push(e);
        }
        let fork = log.last().unwrap().clone();
        log.push(member(&a, &fork, Kind::Remove, &b.key));
        log.push(member(&b, &fork, Kind::Remove, &c.key));
        log.push(member(&c, &fork, Kind::Remove, &a.key));
        let mut parent = fork;
        for extra in 0..=3 {
            let r = replay(&log);
            assert_eq!(
                r.members.keys().collect::<Vec<_>>(),
                [&f.key],
                "{extra} extra"
            );
            let m = op(&f, &parent, Kind::Meta, json!({}));
            log.push(m.clone());
            parent = m;
        }
    }

    #[test]
    fn readmitted_member_writes_again() {
        let (f, x) = (id(), id());
        let g = genesis(&f);
        let ax = admit(&f, &g, &x.key, "x");
        let rx = member(&f, &ax, Kind::Remove, &x.key);
        let again = admit(&f, &rx, &x.key, "x");
        let pr = op(
            &x,
            &again,
            Kind::Proof,
            json!({"member": x.key, "proof": "t"}),
        );
        let mut log = vec![g, ax, rx, again, pr.clone()];
        assert_eq!(replay(&log).members[&x.key].proof.as_deref(), Some("t"));
        log.push(member(&x, &pr, Kind::Leave, &x.key));
        assert!(!replay(&log).members.contains_key(&x.key));
    }

    /// A cloud member of `person`, admitted by `by` with an (opaque) proof.
    fn admit_cloud(
        by: &Identity,
        parent: &Entry,
        who: &Identity,
        name: &str,
        person: &str,
    ) -> Entry {
        let mut b = admit_body(&who.key, name);
        b["computer"] = json!("cloud");
        b["person_id"] = json!(person);
        b["kind"] = json!("cloud");
        b["proof"] = json!("p");
        op(by, parent, Kind::Admit, b)
    }

    #[test]
    fn bridge_member_cannot_link_or_admit() {
        let (f, k, d) = (id(), id(), id());
        let g = genesis(&f);
        let ak = admit_cloud(&f, &g, &k, "bridge", &f.key);
        let pk = member(&f, &ak, Kind::Admin, &k.key);
        let lk = link_with(&k, &pk, "tk", false);
        let kd = admit(&k, &lk, &d.key, "d"); // an admin, but a cloud one
        let r = replay(&[g, ak, pk, lk, kd]);
        assert_eq!(r.members[&k.key].role, Role::Admin);
        assert!(r.links.is_empty());
        assert!(!r.members.contains_key(&d.key));
    }

    #[test]
    fn stale_known_removal_stops_when_its_signers_admission_drops() {
        // Plain member X admits Y through its link; Y unlinks its own cloud member M; the
        // founder removes X concurrently. Y never survives, so neither does its removal of M,
        // whether the founder's removal is shallow or behind entries replayed after Y's.
        let (f, x, y, m) = (id(), id(), id(), id());
        let g = genesis(&f);
        let ax = admit(&f, &g, &x.key, "x");
        let fork = admit_cloud(&f, &ax, &m, "m", &y.key);
        let lx = link_with(&x, &fork, "t", false);
        let xy = admit_via(&x, &lx, &y.key, "y", "t");
        let ym = member(&y, &xy, Kind::Remove, &m.key);
        // M's own entry is concurrent with Y's (stale) removal of M: it must stand.
        let mp = op(
            &m,
            &fork,
            Kind::Proof,
            json!({"member": m.key, "proof": "renewed"}),
        );
        let branch = vec![g, ax, fork.clone(), lx.clone(), xy.clone(), ym, mp];
        let shallow = [
            branch.clone(),
            vec![member(&f, &fork, Kind::Remove, &x.key)],
        ]
        .concat();
        let late = if lx.hash() > xy.hash() { &lx } else { &xy };
        let m1 = op_after(&f, &fork, Kind::Meta, json!({}), late);
        let m2 = op(&f, &m1, Kind::Meta, json!({}));
        let rx = member(&f, &m2, Kind::Remove, &x.key);
        let deep = [branch, vec![m1, m2, rx]].concat();
        let (s, d) = (replay(&shallow), replay(&deep));
        for r in [&s, &d] {
            assert!(!r.members.contains_key(&x.key));
            assert!(!r.members.contains_key(&y.key));
            assert_eq!(r.members[&m.key].proof.as_deref(), Some("renewed"));
        }
        assert_eq!(s.members, d.members);
        assert_eq!(s.links, d.links);
    }

    #[test]
    fn admit_cannot_claim_another_persons_id() {
        let (f, c, k) = (id(), id(), id());
        let g = genesis(&f);
        let mut b = admit_body(&c.key, "c");
        b["person_id"] = json!(f.key);
        let claim = op(&f, &g, Kind::Admit, b);
        let proven = admit_cloud(&f, &claim, &k, "k", &f.key);
        let r = replay(&[g, claim, proven]);
        assert!(!r.members.contains_key(&c.key));
        assert_eq!(r.members[&k.key].person_id, f.key);
    }

    #[test]
    fn duplicate_ticket_id_link_is_dropped() {
        // Z reuses X's `ticket_id` for a once-link and admits a puppet through it: Z's link is
        // dropped, so the puppet is too, and X's link still admits.
        let (f, x, z, p, c) = (id(), id(), id(), id(), id());
        let g = genesis(&f);
        let ax = admit(&f, &g, &x.key, "x");
        let az = admit(&f, &ax, &z.key, "z");
        let lx = link_with(&x, &az, "t", false);
        let lz = link_with(&z, &lx, "t", true);
        let zp = admit_via(&z, &lz, &p.key, "p", "t");
        let xc = admit_via(&x, &zp, &c.key, "c", "t");
        let r = replay(&[g, ax, az, lx, lz, zp, xc]);
        assert!(!r.members.contains_key(&p.key));
        assert!(r.members.contains_key(&c.key));
        assert_eq!(r.links["t"].signer, x.key);
        assert!(!r.links["t"].once);
    }

    #[test]
    fn once_link_admits_exactly_one_even_on_a_fork() {
        let (f, x, c, d) = (id(), id(), id(), id());
        let g = genesis(&f);
        let ax = admit(&f, &g, &x.key, "x");
        let l = link_with(&x, &ax, "t1", true);
        let xc = admit_via(&x, &l, &c.key, "c", "t1");
        let fd = admit_via(&f, &l, &d.key, "d", "t1");
        let mut log = vec![g, ax, l, xc, fd];
        let r = replay(&log);
        assert_eq!(
            r.members.contains_key(&c.key) as u8 + r.members.contains_key(&d.key) as u8,
            1
        );
        assert!(r.links.is_empty());
        log.reverse();
        assert_eq!(replay(&log), r);
    }

    #[test]
    fn prefix_collision_is_refused() {
        let (f, b, other) = (id(), id(), id());
        let g = genesis(&f);
        let ab = admit(&f, &g, &b.key, "b");
        let crafted = format!("{}{}", &b.key[..12], &other.key[12..]);
        let ac = admit(&f, &ab, &crafted, "crafted");
        let r = replay(&[g, ab, ac]);
        assert!(!r.members.contains_key(&crafted));
        assert_eq!(r.members.len(), 2);
    }

    #[test]
    fn name_collision_is_refused() {
        let (f, b, c) = (id(), id(), id());
        let g = genesis(&f);
        let ab = admit(&f, &g, &b.key, "bob");
        let ac = admit(&f, &ab, &c.key, "BOB");
        let r = replay(&[g, ab, ac]);
        assert!(!r.members.contains_key(&c.key));
    }

    #[test]
    fn second_root_and_far_future_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("state.db")).unwrap();
        let (f, other) = (id(), id());
        let now = 1_000;
        let g = genesis(&f);
        assert!(accept(&s, &g, now).unwrap());
        assert!(!accept(&s, &g, now).unwrap());
        assert!(accept(&s, &genesis(&other), now).is_err());
        let e_at = |at| Entry::signed(&f, vec![g.hash()], Kind::Meta, json!({}), at);
        assert!(accept(&s, &e_at(now + 86_401), now).is_err());
        assert!(accept(&s, &e_at(now + 86_400), now).unwrap());
        let orphan = Entry::signed(&f, vec!["00".into()], Kind::Meta, json!({}), now);
        assert!(accept(&s, &orphan, now).is_err());
        let mut forged = e_at(now);
        forged.at += 1;
        assert!(accept(&s, &forged, now).is_err());
    }

    #[test]
    fn founder_leave_promotes_oldest_admin() {
        let (f, b, c) = (id(), id(), id());
        let g = genesis(&f);
        let ab = admit(&f, &g, &b.key, "b");
        let ac = admit(&f, &ab, &c.key, "c");
        let pc = member(&f, &ac, Kind::Admin, &c.key);
        let pb = member(&f, &pc, Kind::Admin, &b.key);
        let fl = member(&f, &pb, Kind::Leave, &f.key);
        let r = replay(&[g, ab, ac, pc, pb, fl]);
        assert!(!r.members.contains_key(&f.key));
        assert_eq!(r.members[&c.key].role, Role::Founder);
        assert_eq!(r.members[&b.key].role, Role::Admin);
    }

    #[test]
    fn transfer_swaps_founder_and_admin() {
        let (f, b) = (id(), id());
        let g = genesis(&f);
        let ab = admit(&f, &g, &b.key, "b");
        let t = member(&f, &ab, Kind::Transfer, &b.key);
        let rf = member(&f, &t, Kind::Remove, &b.key); // naming the new founder: dropped
        let r = replay(&[g, ab, t, rf]);
        assert_eq!(r.members[&b.key].role, Role::Founder);
        assert_eq!(r.members[&f.key].role, Role::Admin);
    }

    #[test]
    fn person_id_is_sticky_and_merge_rewrites_it_once() {
        let (f, b) = (id(), id());
        let g = genesis(&f);
        let ab = admit(&f, &g, &b.key, "b");
        let by_founder = op(
            &f,
            &ab,
            Kind::Merge,
            json!({"member": b.key, "person_id": "p0", "proof": "t"}),
        );
        let m1 = op(
            &b,
            &by_founder,
            Kind::Merge,
            json!({"member": b.key, "person_id": "p1", "proof": "t"}),
        );
        let m2 = op(
            &b,
            &m1,
            Kind::Merge,
            json!({"member": b.key, "person_id": "p2", "proof": "t"}),
        );
        let pr = op(
            &b,
            &m2,
            Kind::Proof,
            json!({"member": b.key, "proof": "t2"}),
        );
        let r = replay(&[g, ab, by_founder, m1, m2, pr]);
        assert_eq!(r.members[&b.key].person_id, "p1");
        assert_eq!(r.members[&b.key].proof.as_deref(), Some("t2"));
    }

    #[test]
    fn inviter_may_admit_through_its_own_link_only() {
        let (f, x, y, c, d, e) = (id(), id(), id(), id(), id(), id());
        let g = genesis(&f);
        let ax = admit(&f, &g, &x.key, "x");
        let ay = admit(&f, &ax, &y.key, "y");
        let lx = link_with(&x, &ay, "tx", false);
        let lf = link_with(&f, &lx, "tf", false);
        let xc = admit_via(&x, &lf, &c.key, "c", "tx");
        let xd = admit_via(&x, &xc, &d.key, "d", "tf");
        let mut log = vec![g, ax, ay, lx, lf, xc, xd];
        let before = replay(&log);
        assert_eq!(before.links.keys().collect::<Vec<_>>(), ["tf", "tx"]);
        assert_eq!(before.links["tx"].signer, x.key);
        let ry = member(&f, log.last().unwrap(), Kind::Remove, &y.key);
        let xe = admit_via(&x, &ry, &e.key, "e", "tx");
        log.extend([ry, xe]);
        let r = replay(&log);
        assert!(r.members.contains_key(&c.key));
        assert!(!r.members.contains_key(&d.key));
        assert!(!r.members.contains_key(&y.key));
        assert!(!r.members.contains_key(&e.key));
        assert!(r.links.is_empty());
    }
}
