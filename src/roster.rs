//! The roster: the team's membership log. A hash-linked DAG of signed entries with one root,
//! replayed deterministically, so every member computes the same team from the same set of
//! entries whatever order they arrived in. Pure: no network, no IPC, no logging.

use crate::identity::{canonical, verify, Identity};
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
        let mut v = self.value();
        v.as_object_mut()
            .expect("an entry is an object")
            .remove("sig");
        HEXLOWER.encode(&Sha256::digest(canonical(&v)))
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
/// and authorised there and no `remove` of its signer precedes or is concurrent with it.
pub fn replay(entries: &[Entry]) -> Roster {
    let by_hash: BTreeMap<String, &Entry> = entries.iter().map(|e| (e.hash(), e)).collect();
    let graph = Roster {
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
    // again knowing the removals the last pass applied, until that set settles.
    // ponytail: capped at one pass per entry; a set that never settles keeps the last pass.
    let mut removals = BTreeSet::new();
    let mut roster = graph.clone();
    for _ in 0..=by_hash.len() {
        let (r, applied) = pass(&by_hash, &anc, &removals, graph.clone());
        roster = r;
        if applied == removals {
            break;
        }
        removals = applied;
    }
    roster
}

/// One replay knowing `known` removals ahead of time; returns the roster and the removals it
/// applied.
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
    let mut removals = known.clone();
    let mut applied = BTreeSet::new();
    let mut links: Vec<(String, Link)> = Vec::new();
    let mut admins: Vec<String> = Vec::new();
    // The heads are the entries no entry names as a parent.
    r.heads = by_hash
        .keys()
        .filter(|h| !children.contains_key(h.as_str()))
        .cloned()
        .collect();

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
        let Some(signer) = r.members.get(&e.signer).cloned() else {
            continue;
        };
        // A removal of the signer that this entry does not precede drops it.
        if removals.iter().any(|rh| {
            *rh != h && by_hash[rh].text("member") == e.signer && !anc[rh.as_str()].contains(&h)
        }) {
            continue;
        }
        let elevated = signer.role != Role::Member;
        let target = e.text("member").to_string();
        match e.kind {
            Kind::Admit => {
                let via_link = || {
                    links.iter().any(|(lh, l)| {
                        l.ticket_id == e.text("ticket_id")
                            && l.signer == e.signer
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
                if (elevated || via_link()) && !collides && role_ok {
                    r.members.insert(target, new_member(e, Role::Member));
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
                if t.role != Role::Founder && (elevated || unlink) {
                    r.members.remove(&target);
                    removals.insert(h.clone());
                    applied.insert(h.clone());
                }
            }
            Kind::Admin => {
                if signer.role == Role::Founder {
                    if let Some(t) = r.members.get_mut(&target) {
                        if t.role == Role::Member {
                            t.role = Role::Admin;
                            admins.push(target);
                        }
                    }
                }
            }
            Kind::Transfer => {
                if signer.role == Role::Founder && target != e.signer {
                    if let Some(t) = r.members.get_mut(&target) {
                        t.role = Role::Founder;
                        r.members.get_mut(&e.signer).expect("active").role = Role::Admin;
                        admins.push(e.signer.clone());
                    }
                }
            }
            Kind::Leave => {
                if target == e.signer {
                    r.members.remove(&target);
                    if signer.role == Role::Founder {
                        let oldest = admins
                            .iter()
                            .find(|a| r.members.get(*a).is_some_and(|m| m.role == Role::Admin));
                        if let Some(a) = oldest.cloned() {
                            r.members.get_mut(&a).expect("active").role = Role::Founder;
                        }
                    }
                }
            }
            Kind::Link => links.push((
                h.clone(),
                Link {
                    ticket_id: e.text("ticket_id").to_string(),
                    secret: e.text("secret").to_string(),
                    mode: e.text("mode").to_string(),
                    once: e.body["once"].as_bool().unwrap_or(false),
                    expires_at: e.body["expires_at"].as_i64().unwrap_or(0),
                    signer: e.signer.clone(),
                },
            )),
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
        }
    }
    // Live links: those no applied removal descends from.
    for (lh, l) in links {
        if !applied.iter().any(|rh| anc[rh.as_str()].contains(&lh)) {
            r.links.insert(l.ticket_id.clone(), l);
        }
    }
    (r, applied)
}

fn is_genesis(e: &Entry) -> bool {
    e.kind == Kind::Admit
        && e.text("role") == "founder"
        && e.text("member") == e.signer
        && e.signer.len() >= 12
}

fn new_member(e: &Entry, role: Role) -> Member {
    let key = e.text("member").to_string();
    let person_id = match e.text("person_id") {
        "" => key.clone(),
        p => p.to_string(),
    };
    Member {
        short: key[..12].to_string(),
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
    // ponytail: reads the whole log per entry; query by hash if rosters grow large.
    let stored = entries(store)?;
    let hashes: HashSet<String> = stored.iter().map(Entry::hash).collect();
    if hashes.contains(&hash) {
        return Ok(false);
    }
    if entry.parents.iter().any(|p| !hashes.contains(p)) {
        return Err("roster entry names an unknown parent".into());
    }
    if entry.parents.is_empty() && stored.iter().any(|e| e.parents.is_empty()) {
        return Err("the roster already has a root".into());
    }
    store.conn().execute(
        "INSERT INTO roster (hash, entry, received_at) VALUES (?1, ?2, ?3)",
        params![hash, serde_json::to_string(entry)?, now],
    )?;
    Ok(true)
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

    fn genesis(f: &Identity) -> Entry {
        Entry::signed(
            f,
            vec![],
            Kind::Admit,
            json!({"member": f.key, "name": "founder", "computer": "mac", "person_id": f.key,
                   "kind": "computer", "role": "founder", "ticket_id": null, "team_name": "acme"}),
            1,
        )
    }

    fn admit_via(by: &Identity, parent: &Entry, who: &str, name: &str, ticket: &str) -> Entry {
        op(
            by,
            parent,
            Kind::Admit,
            json!({"member": who, "name": name, "computer": "mac", "person_id": who,
                   "kind": "computer", "role": "member", "ticket_id": ticket}),
        )
    }

    fn admit(by: &Identity, parent: &Entry, who: &str, name: &str) -> Entry {
        admit_via(by, parent, who, name, "")
    }

    fn member(by: &Identity, parent: &Entry, kind: Kind, who: &str) -> Entry {
        op(by, parent, kind, json!({ "member": who }))
    }

    fn link(by: &Identity, parent: &Entry, ticket: &str) -> Entry {
        op(
            by,
            parent,
            Kind::Link,
            json!({"ticket_id": ticket, "secret": "s", "mode": "ask", "once": false, "expires_at": 99}),
        )
    }

    /// Same parent: X admits C | founder removes X.
    fn fork() -> (Vec<Entry>, [String; 2]) {
        let (f, x, c) = (id(), id(), id());
        let g = genesis(&f);
        let ax = admit(&f, &g, &x.key, "x");
        let xc = admit(&x, &ax, &c.key, "c");
        let rx = member(&f, &ax, Kind::Remove, &x.key);
        (vec![g, ax, xc, rx], [x.key, c.key])
    }

    /// Fork at F: X admits C, then X admits D | founder removes X.
    fn deep_fork() -> (Vec<Entry>, [String; 3]) {
        let (f, x, c, d) = (id(), id(), id(), id());
        let g = genesis(&f);
        let ax = admit(&f, &g, &x.key, "x");
        let xc = admit(&x, &ax, &c.key, "c");
        let xd = admit(&x, &xc, &d.key, "d");
        let rx = member(&f, &ax, Kind::Remove, &x.key);
        (vec![g, ax, xc, xd, rx], [x.key, c.key, d.key])
    }

    fn permutations(v: &[Entry]) -> Vec<Vec<Entry>> {
        if v.len() <= 1 {
            return vec![v.to_vec()];
        }
        let mut out = Vec::new();
        for i in 0..v.len() {
            let mut rest = v.to_vec();
            let first = rest.remove(i);
            for mut p in permutations(&rest) {
                p.insert(0, first.clone());
                out.push(p);
            }
        }
        out
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
        // Branch 2 reaches the removal only after two meta entries, so branch 1's admission
        // may be replayed first; it must still end up dropped.
        let (f, x, c) = (id(), id(), id());
        let g = genesis(&f);
        let ax = admit(&f, &g, &x.key, "x");
        let xc = admit(&x, &ax, &c.key, "c");
        let m1 = op(&f, &ax, Kind::Meta, json!({"relay": "r1"}));
        let m2 = op(&f, &m1, Kind::Meta, json!({"team_name": "acme2"}));
        let rx = member(&f, &m2, Kind::Remove, &x.key);
        let r = replay(&[g, ax, xc, m1, m2, rx]);
        assert_eq!(r.members.len(), 1);
        assert_eq!(r.team_name, "acme2");
        assert_eq!(r.relay.as_deref(), Some("r1"));
    }

    #[test]
    fn replay_is_order_independent() {
        for entries in [fork().0, deep_fork().0] {
            let want = replay(&entries);
            for p in permutations(&entries) {
                assert_eq!(replay(&p), want);
            }
        }
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
        let lx = link(&x, &ay, "tx");
        let lf = link(&f, &lx, "tf");
        let xc = admit_via(&x, &lf, &c.key, "c", "tx");
        let xd = admit_via(&x, &xc, &d.key, "d", "tf");
        let before = replay(&[
            g.clone(),
            ax.clone(),
            ay.clone(),
            lx.clone(),
            lf.clone(),
            xc.clone(),
            xd.clone(),
        ]);
        assert_eq!(before.links.keys().collect::<Vec<_>>(), ["tf", "tx"]);
        assert_eq!(before.links["tx"].signer, x.key);
        let ry = member(&f, &xd, Kind::Remove, &y.key);
        let xe = admit_via(&x, &ry, &e.key, "e", "tx");
        let r = replay(&[g, ax, ay, lx, lf, xc, xd, ry, xe]);
        assert!(r.members.contains_key(&c.key));
        assert!(!r.members.contains_key(&d.key));
        assert!(!r.members.contains_key(&y.key));
        assert!(!r.members.contains_key(&e.key));
        assert!(r.links.is_empty());
    }
}
