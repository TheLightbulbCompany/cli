//! The roster: the team's membership log. A hash-linked DAG of signed entries with one root,
//! replayed deterministically, so every member computes the same team from the same set of
//! entries whatever order they arrived in. Pure: no network, no IPC, no logging.

use crate::identity::{canonical, unsigned, verify, Identity};
use crate::proof::{self, Proof, ProofKind};
use crate::store::Store;
use crate::Result;
use data_encoding::HEXLOWER;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// How far ahead of our clock an entry may be stamped.
const MAX_SKEW_S: i64 = 86_400;
/// An entry's canonical JSON may not pass this (a proof is at most 4 KiB): one entry must
/// always fit a `roster` frame, or sync with that peer would fail for good.
pub const MAX_ENTRY_BYTES: usize = 16 * 1024;

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

/// Ordered by authority: `Member < Admin < Founder`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Member {
    pub key: String,
    pub short: String,
    pub name: String,
    pub computer: String,
    pub person_id: String,
    pub role: Role,
    pub kind: MemberKind,
    pub admitted_at: i64,
    /// The latest proof that verified for this member (`proof::parse`); `None` once it
    /// expired (`Roster::lapse`).
    pub proof: Option<Proof>,
    /// `proof`'s `workspace.id`: the workspace the person is signed into (the workspace
    /// member's own); `None` with no proof in force. The admit body's `workspace` is ignored.
    pub workspace: Option<String>,
}

impl Member {
    /// Takes `p` as this member's proof, and its workspace.
    fn prove(&mut self, p: Proof) {
        self.workspace = p.workspace.as_ref().map(|w| w.id.clone());
        self.proof = Some(p);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Collision {
    Name,
    Key,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub ticket_id: String,
    pub secret: String,
    pub mode: String,
    pub once: bool,
    pub expires_at: i64,
    pub signer: String,
    /// `invite --new`'s link: it ends every earlier link of its signer, any mode, once ones too.
    pub ends_all: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Roster {
    pub team_id: String,
    pub team_name: String,
    /// The genesis entry's stamp.
    pub created_at: i64,
    pub members: BTreeMap<String, Member>,
    pub heads: Vec<String>,
    pub links: BTreeMap<String, Link>,
    pub relay: Option<String>,
    parents: BTreeMap<String, Vec<String>>,
    /// Every replayed entry with its hash, parents first.
    log: Vec<(String, Entry)>,
}

impl Roster {
    /// `key`'s person: its member's `person_id`, else `key` itself.
    pub fn person_of<'a>(&'a self, key: &'a str) -> &'a str {
        self.members.get(key).map_or(key, |m| &m.person_id)
    }

    /// Whether `b` is a computer member of `a`'s person.
    pub fn same_person_computer(&self, a: &str, b: &str) -> bool {
        match (self.members.get(a), self.members.get(b)) {
            (Some(a), Some(b)) => b.kind == MemberKind::Computer && a.person_id == b.person_id,
            _ => false,
        }
    }

    pub fn founder(&self) -> Option<&Member> {
        self.members.values().find(|m| m.role == Role::Founder)
    }

    /// Whether `me` is this roster's only member: a computer on its own network.
    pub fn is_solo(&self, me: &str) -> bool {
        self.members.len() == 1 && self.members.contains_key(me)
    }

    /// What an admission of `key` called `name` would collide with: a member's name (any
    /// case) other than one of person `same` (another computer of a signed-in person goes by
    /// their name), else a member's 12-character prefix. The one rule for replay and admission.
    pub fn collides(&self, name: &str, key: &str, same: Option<&str>) -> Option<Collision> {
        let name = name.to_lowercase();
        let prefix = key.get(..12);
        let ms = || self.members.values();
        let other = |m: &Member| same != Some(m.person_id.as_str());
        if ms().any(|m| other(m) && m.name.to_lowercase() == name) {
            Some(Collision::Name)
        } else if ms().any(|m| Some(m.short.as_str()) == prefix) {
            Some(Collision::Key)
        } else {
            None
        }
    }

    /// My workspace's member: the cloud member whose `person_id` is `ws:<me.workspace>`, while
    /// its own proof is in force (a lapsed one has no authority: app sends, input grants,
    /// `agents_send_without_asking`).
    pub fn my_workspace_member(&self, me: &Member) -> Option<&Member> {
        let ws = format!("ws:{}", me.workspace.as_ref()?);
        self.members
            .values()
            .find(|m| m.kind == MemberKind::Cloud && m.person_id == ws && m.workspace.is_some())
    }

    /// Person `person`'s computer members, first admitted first.
    pub fn computers_of(&self, person: &str) -> Vec<&Member> {
        let mut ms: Vec<&Member> = (self.members.values())
            .filter(|m| m.kind == MemberKind::Computer && m.person_id == person)
            .collect();
        ms.sort_by_key(|m| (m.admitted_at, &m.key));
        ms
    }

    /// Drops every proof expired at `now`, and the workspace it named; `person_id` stays.
    /// Run on every install and on the daemon's timer: replay holds no clock.
    pub fn lapse(&mut self, now: i64) {
        for m in self.members.values_mut() {
            if m.proof.as_ref().is_some_and(|p| now > p.exp) {
                m.proof = None;
                m.workspace = None;
            }
        }
    }

    pub fn by_short(&self, short: &str) -> Option<&Member> {
        self.members.values().find(|m| m.short == short)
    }

    /// Whether `hash` is one of the replayed entries.
    pub fn knows(&self, hash: &str) -> bool {
        self.parents.contains_key(hash)
    }

    /// Every replayed entry with its hash, parents first.
    pub fn log(&self) -> &[(String, Entry)] {
        &self.log
    }

    /// Every entry one of `hashes` descends from (a start only if another descends from it).
    pub fn ancestors_of<S: AsRef<str>>(&self, hashes: &[S]) -> HashSet<String> {
        let mut seen = HashSet::new();
        let mut todo: Vec<&str> = hashes.iter().map(AsRef::as_ref).collect();
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
        .map(|h| (h.as_str(), graph.ancestors_of(&[h])))
        .collect();
    // A removal deeper in its branch than a concurrent entry is replayed after it, so replay
    // again knowing every removal any pass applied: each drops its target's concurrent entries
    // ahead of its own position. `known` only grows, so this settles within one pass per
    // removal. ponytail: the cap (#removes + 1) is a safety only.
    let cap = by_hash.values().filter(|e| e.kind == Kind::Remove).count() + 1;
    // Each proof verified once, not once per pass.
    let proofs: HashMap<&str, Proof> = by_hash
        .iter()
        .filter_map(|(h, e)| Some((h.as_str(), proven(e)?)))
        .collect();
    let pass =
        |known: &BTreeSet<String>, graph: Roster| pass(&by_hash, &anc, &proofs, known, graph);
    let mut known = BTreeSet::new();
    let (mut r, mut applied) = pass(&known, graph.clone());
    for _ in 1..cap {
        if applied.is_subset(&known) {
            break;
        }
        known.extend(applied.iter().cloned());
        (r, applied) = pass(&known, graph.clone());
    }
    // A known removal whose signer's admission no longer survives is stale: replay once more
    // knowing only the removals that still apply, and keep that if it agrees with itself.
    if applied != known {
        let (exact, again) = pass(&applied, graph);
        if again == applied {
            r = exact;
        }
    }
    r.log = ordered(&by_hash);
    r
}

/// One replay in which the `known` removals drop their targets' concurrent entries ahead of
/// their own position; returns the roster and the removals it applied. `proofs`: each entry's
/// verified `proof`.
fn pass(
    by_hash: &BTreeMap<String, &Entry>,
    anc: &BTreeMap<&str, HashSet<String>>,
    proofs: &HashMap<&str, Proof>,
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
        let claims = proofs.get(h.as_str());
        if r.team_id.is_empty() {
            if e.parents.is_empty() && is_genesis(e, claims) {
                r.team_id = h.clone();
                r.team_name = e.text("team_name").to_string();
                r.created_at = e.at;
                r.members
                    .insert(e.signer.clone(), new_member(e, Role::Founder, claims));
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
                            && !retired(&links, anc, lh, l, |n| anc[h.as_str()].contains(n))
                            // Every removal the link does not descend from ends it; an
                            // admission it already knew of stands.
                            && removals.iter().all(|rh| {
                                anc[lh.as_str()].contains(rh) || anc[rh.as_str()].contains(&h)
                            })
                    })
                };
                if target.get(..12).is_none() {
                    continue;
                }
                let cloud = e.text("kind") == "cloud";
                // Another computer of a signed-in person: admitted by one of that person's.
                let same = (!cloud && claims.is_some()).then(|| e.text("person_id"));
                let same_person = same.is_some_and(|p| signer.person_id == p);
                let collides = r.collides(e.text("name"), &target, same).is_some();
                let role_ok = matches!(
                    e.body.get("role").and_then(Value::as_str),
                    None | Some("member")
                );
                // A link's signer admits only computers; a cloud member needs an admin, and a
                // workspace has one: on a fork, the first replayed stands.
                let taken = cloud
                    && r.members
                        .values()
                        .any(|m| m.kind == MemberKind::Cloud && m.person_id == e.text("person_id"));
                let kind_ok = (elevated || !cloud) && !taken;
                let authorised = elevated || via_link() || same_person;
                if authorised && kind_ok && !collides && role_ok && own_person(e, claims) {
                    r.members
                        .insert(target, new_member(e, Role::Member, claims));
                    if let Some((lh, _)) = link.filter(|(_, l)| l.once) {
                        used_once.insert(lh.clone());
                    }
                }
            }
            // Sign-in: the computer's own, once, to the person its proof names.
            Kind::Merge => {
                let m = r.members.get_mut(&target);
                let p = claims.filter(|p| p.kind == ProofKind::Computer);
                if let (Some(m), Some(p)) = (m, p) {
                    let to_own = target == e.signer && m.kind == MemberKind::Computer;
                    if to_own && m.person_id == m.key && p.sub == e.text("person_id") {
                        m.person_id = p.sub.clone();
                        m.prove(p.clone());
                    }
                }
            }
            // A renewal: a proof of the member's own person and kind.
            Kind::Proof => {
                let m = r.members.get_mut(&target).filter(|_| target == e.signer);
                if let (Some(m), Some(p)) = (m, claims) {
                    let kind = match m.kind {
                        MemberKind::Computer => ProofKind::Computer,
                        MemberKind::Cloud => ProofKind::Workspace,
                    };
                    // The newest stands, whatever order a fork replays in.
                    let newer = m.proof.as_ref().is_none_or(|c| p.iat >= c.iat);
                    if p.sub == m.person_id && p.kind == kind && newer {
                        m.prove(p.clone());
                    }
                }
            }
            Kind::Remove => {
                let Some(t) = r.members.get(&target) else {
                    continue;
                };
                // `unlink` (retired): a member removing its own person's cloud member. It can no
                // longer fire: a cloud member's person is `ws:<id>`, never a computer's.
                let unlink = t.kind == MemberKind::Cloud && t.person_id == signer.person_id;
                if (elevated || unlink) && take(&mut r, &mut gone, &target, &h) {
                    removals.insert(h.clone());
                    applied.insert(h.clone());
                }
            }
            // A cloud member is never made an admin or the founder.
            Kind::Admin if founder => {
                if let Some(t) = r.members.get_mut(&target) {
                    if t.role == Role::Member && t.kind == MemberKind::Computer {
                        t.role = Role::Admin;
                        admins.push(target);
                    }
                }
            }
            Kind::Transfer if founder && target != e.signer => {
                let to = r.members.get_mut(&target);
                if let Some(t) = to.filter(|t| t.kind == MemberKind::Computer) {
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
            // Only a computer member mints links, under its own `<short>.` prefix, and a
            // `ticket_id` names one link ever; an `open` one only the founder and admins.
            Kind::Link
                if computer
                    && (elevated || e.text("mode") != "open")
                    && e.text("ticket_id")
                        .strip_prefix(signer.short.as_str())
                        .is_some_and(|rest| rest.len() > 1 && rest.starts_with('.')) =>
            {
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
                            ends_all: e.body["ends_all"] == true,
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
    // Live links: unused, not retired, and descended from every removal (a removal it did not
    // see ends it).
    for (lh, l) in links.values() {
        if !used_once.contains(lh)
            && !retired(&links, anc, lh, l, |_| true)
            && removals.iter().all(|rh| anc[lh.as_str()].contains(rh))
        {
            r.links.insert(l.ticket_id.clone(), l.clone());
        }
    }
    (r, applied)
}

/// Whether link `l` (entry `lh`) is ended by a newer link of its signer that `seen` holds (one
/// descended from it): a reusable one in the same mode ends a reusable one; an `ends_all` one
/// ends every link. Otherwise once links never end or are ended.
fn retired(
    links: &BTreeMap<String, (String, Link)>,
    anc: &BTreeMap<&str, HashSet<String>>,
    lh: &str,
    l: &Link,
    seen: impl Fn(&str) -> bool,
) -> bool {
    links.values().any(|(nh, n)| {
        n.signer == l.signer
            && (n.ends_all || (!l.once && !n.once && n.mode == l.mode))
            && anc[nh.as_str()].contains(lh)
            && seen(nh)
    })
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

/// An `admit` names the member's own key as its `person_id`, or the `sub` of its verified
/// proof (the one way to claim another id); a cloud member's needs a `workspace` proof, a
/// computer's a `computer` one when it has any.
fn own_person(e: &Entry, claims: Option<&Proof>) -> bool {
    let cloud = e.text("kind") == "cloud";
    match claims {
        Some(p) => p.sub == e.text("person_id") && (p.kind == ProofKind::Workspace) == cloud,
        None => !cloud && e.text("person_id") == e.text("member"),
    }
}

/// The verified (whatever the time) proof `e.body.proof` for `e.body.member`.
fn proven(e: &Entry) -> Option<Proof> {
    let token = e.body["proof"].as_str()?;
    proof::parse(token, e.text("member")).ok()
}

fn is_genesis(e: &Entry, claims: Option<&Proof>) -> bool {
    e.kind == Kind::Admit
        && own_person(e, claims)
        && e.text("role") == "founder"
        && e.text("member") == e.signer
        && e.signer.get(..12).is_some()
}

fn new_member(e: &Entry, role: Role, claims: Option<&Proof>) -> Member {
    let key = e.text("member").to_string();
    let person_id = match e.text("person_id") {
        "" => key.clone(),
        p => p.to_string(),
    };
    let mut m = Member {
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
        proof: None,
        workspace: None,
        key,
    };
    if let Some(p) = claims {
        m.prove(p.clone());
    }
    m
}

/// `by_hash` with every entry after its parents (lowest hash first among those ready); an
/// entry whose parent is missing is left out.
fn ordered(by_hash: &BTreeMap<String, &Entry>) -> Vec<(String, Entry)> {
    let mut waiting: BTreeMap<&str, usize> = BTreeMap::new();
    let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (h, e) in by_hash {
        waiting.insert(h, e.parents.len());
        for p in &e.parents {
            children.entry(p.as_str()).or_default().push(h);
        }
    }
    let mut ready: BTreeSet<&str> = waiting
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(h, _)| *h)
        .collect();
    let mut out = Vec::new();
    while let Some(h) = ready.pop_first() {
        for c in children.remove(h).into_iter().flatten() {
            let n = waiting.get_mut(c).expect("a child is an entry");
            *n -= 1;
            if *n == 0 {
                ready.insert(c);
            }
        }
        out.push((h.to_string(), by_hash[h].clone()));
    }
    out
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
    if canonical(&entry.value()).len() > MAX_ENTRY_BYTES {
        return Err("roster entry is too large".into());
    }
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
    use crate::proof::test_issuer::{computer_proof, workspace_proof};
    use serde_json::json;

    /// Proofs are issued at this time (replay holds no clock).
    const IAT: i64 = 1_800_000_000;

    /// `who`'s computer proof as person `sub`, signed into workspace `ws`.
    fn cp(who: &Identity, sub: &str, ws: Option<&str>) -> String {
        computer_proof(
            &who.key,
            sub,
            "Prasiddha",
            ws.map(|w| (w, "The Lightbulb Company")),
            IAT,
        )
    }

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

    /// `by`'s ticket id `t`: prefixed by its signer's short, as replay requires.
    fn tid(by: &Identity, t: &str) -> String {
        format!("{}.{t}", by.short)
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
            Kind::Merge,
            json!({"member": x.key, "person_id": "user_x", "proof": cp(&x, "user_x", None)}),
        );
        let mut log = vec![g, ax, rx, again, pr.clone()];
        assert_eq!(replay(&log).members[&x.key].person_id, "user_x");
        log.push(member(&x, &pr, Kind::Leave, &x.key));
        assert!(!replay(&log).members.contains_key(&x.key));
    }

    /// The workspace member of workspace `ws` (person `ws:<ws>`), admitted by `by` with its
    /// workspace proof.
    fn admit_cloud(by: &Identity, parent: &Entry, who: &Identity, name: &str, ws: &str) -> Entry {
        let mut b = admit_body(&who.key, name);
        b["computer"] = json!("cloud");
        b["person_id"] = json!(format!("ws:{ws}"));
        b["kind"] = json!("cloud");
        b["proof"] = json!(workspace_proof(&who.key, ws, name, &["Friday"], IAT));
        op(by, parent, Kind::Admit, b)
    }

    #[test]
    fn bridge_member_cannot_link_or_admit() {
        let (f, k, d) = (id(), id(), id());
        let g = genesis(&f);
        let ak = admit_cloud(&f, &g, &k, "bridge", "W1");
        let pk = member(&f, &ak, Kind::Admin, &k.key);
        let lk = link_with(&k, &pk, &tid(&k, "tk"), false);
        let kd = admit(&k, &lk, &d.key, "d");
        let r = replay(&[g, ak, pk, lk, kd]);
        assert!(r.links.is_empty());
        assert!(!r.members.contains_key(&d.key));
    }

    #[test]
    fn one_cloud_member_per_workspace_even_on_a_fork() {
        let (f, k1, k2) = (id(), id(), id());
        let g = genesis(&f);
        let a1 = admit_cloud(&f, &g, &k1, "ws one", "W1");
        let a2 = admit_cloud(&f, &g, &k2, "ws two", "W1");
        let mut log = vec![g, a1, a2];
        let r = replay(&log);
        let clouds: Vec<_> = r
            .members
            .values()
            .filter(|m| m.person_id == "ws:W1")
            .collect();
        assert_eq!(clouds.len(), 1);
        log.reverse();
        assert_eq!(replay(&log).members, r.members);
    }

    #[test]
    fn cloud_member_cannot_be_admin_or_founder() {
        let (f, k) = (id(), id());
        let g = genesis(&f);
        let ak = admit_cloud(&f, &g, &k, "bridge", "W1");
        let pk = member(&f, &ak, Kind::Admin, &k.key);
        let tk = member(&f, &pk, Kind::Transfer, &k.key);
        let r = replay(&[g, ak, pk, tk]);
        assert_eq!(r.members[&k.key].role, Role::Member);
        assert_eq!(r.members[&f.key].role, Role::Founder);
    }

    #[test]
    fn stale_known_removal_stops_when_its_signers_admission_drops() {
        // Plain member X admits Y through its link; Y (an admin's equal there) removes the
        // workspace member M; the founder removes X concurrently. Y never survives, so neither
        // does its removal of M, whether the founder's removal is shallow or behind entries
        // replayed after Y's.
        let (f, x, y, m) = (id(), id(), id(), id());
        let g = genesis(&f);
        let ax = admit(&f, &g, &x.key, "x");
        let fork = admit_cloud(&f, &ax, &m, "m", "W1");
        let lx = link_with(&x, &fork, &tid(&x, "t"), false);
        let xy = admit_via(&x, &lx, &y.key, "y", &tid(&x, "t"));
        let ym = member(&y, &xy, Kind::Remove, &m.key);
        // M's own entry is concurrent with Y's (stale) removal of M: it must stand.
        let renewed = workspace_proof(&m.key, "W1", "m", &["Friday"], IAT + 3600);
        let mp = op(
            &m,
            &fork,
            Kind::Proof,
            json!({"member": m.key, "proof": renewed}),
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
            assert_eq!(r.members[&m.key].proof.as_ref().unwrap().iat, IAT + 3600);
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
        let claim = op(&f, &g, Kind::Admit, b.clone());
        // A proof for another computer, or naming another person, claims nothing either.
        b["person_id"] = json!("user_c");
        b["proof"] = json!(cp(&k, "user_c", None));
        let foreign = op(&f, &claim, Kind::Admit, b.clone());
        b["proof"] = json!(cp(&c, "user_d", None));
        let other = op(&f, &foreign, Kind::Admit, b);
        let proven = admit_cloud(&f, &other, &k, "k", "W1");
        let r = replay(&[g, claim, foreign, other, proven]);
        assert!(!r.members.contains_key(&c.key));
        assert_eq!(r.members[&k.key].person_id, "ws:W1");
    }

    #[test]
    fn duplicate_ticket_id_link_is_dropped() {
        // Z reuses X's `ticket_id` for a once-link and admits a puppet through it: Z's link is
        // dropped, so the puppet is too, and X's link still admits.
        let (f, x, z, p, c) = (id(), id(), id(), id(), id());
        let g = genesis(&f);
        let ax = admit(&f, &g, &x.key, "x");
        let az = admit(&f, &ax, &z.key, "z");
        let t = tid(&x, "t");
        let lx = link_with(&x, &az, &t, false);
        let lz = link_with(&z, &lx, &t, true);
        let zp = admit_via(&z, &lz, &p.key, "p", &t);
        let xc = admit_via(&x, &zp, &c.key, "c", &t);
        let r = replay(&[g, ax, az, lx, lz, zp, xc]);
        assert!(!r.members.contains_key(&p.key));
        assert!(r.members.contains_key(&c.key));
        assert_eq!(r.links[&t].signer, x.key);
        assert!(!r.links[&t].once);
    }

    #[test]
    fn once_link_admits_exactly_one_even_on_a_fork() {
        let (f, x, c, d) = (id(), id(), id(), id());
        let g = genesis(&f);
        let ax = admit(&f, &g, &x.key, "x");
        let t = tid(&x, "t1");
        let l = link_with(&x, &ax, &t, true);
        let xc = admit_via(&x, &l, &c.key, "c", &t);
        let fd = admit_via(&f, &l, &d.key, "d", &t);
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
    fn oversize_entry_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("state.db")).unwrap();
        let f = id();
        let now = 1_000;
        let g = genesis(&f);
        assert!(accept(&s, &g, now).unwrap());
        let padded = |n: usize| {
            let body = json!({ "pad": "x".repeat(n) });
            Entry::signed(&f, vec![g.hash()], Kind::Meta, body, now)
        };
        let fits = padded(MAX_ENTRY_BYTES - 1024);
        assert!(canonical(&fits.value()).len() <= MAX_ENTRY_BYTES);
        assert!(accept(&s, &fits, now).unwrap());
        let over = padded(MAX_ENTRY_BYTES);
        assert!(accept(&s, &over, now).is_err());
        assert!(!s
            .conn()
            .prepare("SELECT 1 FROM roster WHERE hash = ?1")
            .unwrap()
            .exists([over.hash()])
            .unwrap());
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

    fn merge(by: &Identity, parent: &Entry, who: &Identity, pid: &str, proof: &str) -> Entry {
        let body = json!({"member": who.key, "person_id": pid, "proof": proof});
        op(by, parent, Kind::Merge, body)
    }

    #[test]
    fn merge_switches_person_id_once_and_expiry_keeps_it() {
        let (f, b) = (id(), id());
        let g = genesis(&f);
        let ab = admit(&f, &g, &b.key, "b");
        let by_founder = merge(&f, &ab, &b, "p0", &cp(&b, "p0", None));
        let m1 = merge(&b, &by_founder, &b, "p1", &cp(&b, "p1", None));
        let m2 = merge(&b, &m1, &b, "p2", &cp(&b, "p2", None));
        let renewed = computer_proof(&b.key, "p1", "Prasiddha", None, IAT + 3600);
        let pr = op(
            &b,
            &m2,
            Kind::Proof,
            json!({"member": b.key, "proof": renewed}),
        );
        // Another person's renewal is dropped.
        let pr2 = op(
            &b,
            &pr,
            Kind::Proof,
            json!({"member": b.key, "proof": cp(&b, "p2", None)}),
        );
        // An older renewal concurrent with the newer one does not replace it.
        let stale = computer_proof(&b.key, "p1", "Prasiddha", None, IAT + 60);
        let pr3 = op(
            &b,
            &m2,
            Kind::Proof,
            json!({"member": b.key, "proof": stale}),
        );
        let mut r = replay(&[g, ab, by_founder, m1, m2, pr, pr2, pr3]);
        assert_eq!(r.members[&b.key].person_id, "p1");
        assert_eq!(r.members[&b.key].proof.as_ref().unwrap().iat, IAT + 3600);
        r.lapse(IAT + 3600 + 86_401);
        assert_eq!(r.members[&b.key].person_id, "p1");
        assert!(r.members[&b.key].proof.is_none());
    }

    #[test]
    fn merge_claiming_another_persons_id_is_dropped() {
        let (f, x, y) = (id(), id(), id());
        let g = genesis(&f);
        let ax = admit(&f, &g, &x.key, "x");
        let ay = admit(&f, &ax, &y.key, "y");
        let my = merge(&y, &ay, &y, "user_y", &cp(&y, "user_y", None));
        // X's valid proof names X's person, not Y's.
        let bad = merge(&x, &my, &x, "user_y", &cp(&x, "user_x", None));
        let r = replay(&[g.clone(), ax.clone(), ay.clone(), my.clone(), bad.clone()]);
        assert_eq!(r.members[&x.key].person_id, x.key);
        let good = merge(&x, &bad, &x, "user_x", &cp(&x, "user_x", None));
        let r = replay(&[g, ax, ay, my, bad, good]);
        assert_eq!(r.members[&x.key].person_id, "user_x");
        assert_eq!(r.members[&y.key].person_id, "user_y");
    }

    #[test]
    fn member_workspace_comes_from_the_proof_and_lapses_with_it() {
        let (f, x) = (id(), id());
        let g = genesis(&f);
        let mut b = admit_body(&x.key, "x");
        b["person_id"] = json!("user_x");
        b["workspace"] = json!("W2");
        b["proof"] = json!(cp(&x, "user_x", Some("W1")));
        let ax = op(&f, &g, Kind::Admit, b);
        let mut r = replay(&[g, ax]);
        assert_eq!(r.members[&x.key].workspace.as_deref(), Some("W1"));
        r.lapse(IAT + 86_400);
        assert_eq!(
            r.members[&x.key].workspace.as_deref(),
            Some("W1"),
            "in force to exp"
        );
        r.lapse(IAT + 86_401);
        assert_eq!(r.members[&x.key].workspace, None);
        assert_eq!(r.members[&x.key].person_id, "user_x");
    }

    #[test]
    fn link_concurrent_with_a_removal_is_ended() {
        // Admin A removes X without having seen the links: both end. P's admission through
        // its link that the removal knew of stands; one concurrent with it drops.
        let (f, a, x, p, c, d) = (id(), id(), id(), id(), id(), id());
        let g = genesis(&f);
        let [aa0, aa] = admit_admin(&f, &g, &a);
        let ax = admit(&f, &aa, &x.key, "m"); // `admit_admin` names A "x"
        let ap = admit(&f, &ax, &p.key, "p");
        let lf = link_with(&f, &ap, &tid(&f, "tf"), false);
        let tp = tid(&p, "tp");
        let lp = link_with(&p, &lf, &tp, false);
        let pc = admit_via(&p, &lp, &c.key, "c", &tp);
        let rx = member(&a, &ap, Kind::Remove, &x.key);
        let base = vec![g, aa0, aa, ax, ap.clone(), lf.clone(), lp.clone()];
        let r = replay(&[base.clone(), vec![pc.clone(), rx]].concat());
        assert!(!r.members.contains_key(&x.key));
        assert!(!r.members.contains_key(&c.key));
        assert!(r.links.is_empty());
        // The same removal behind entries that sort after the links and C's admission: the
        // first pass admits C, and only the removal reaching it through `known` drops it.
        let late = [&lf, &lp, &pc]
            .into_iter()
            .max_by_key(|e| e.hash())
            .unwrap();
        let m1 = op_after(&f, &ap, Kind::Meta, json!({}), late);
        let m2 = op(&f, &m1, Kind::Meta, json!({}));
        let rx_deep = member(&a, &m2, Kind::Remove, &x.key);
        let r = replay(&[base.clone(), vec![pc, m1, m2, rx_deep]].concat());
        assert!(!r.members.contains_key(&x.key));
        assert!(!r.members.contains_key(&c.key));
        assert!(r.links.is_empty());
        // The removal descends from P's admission of D: D stands, the link still ends.
        let pd = admit_via(&p, &lp, &d.key, "d", &tp);
        let rx2 = member(&a, &pd, Kind::Remove, &x.key);
        let r = replay(&[base, vec![pd, rx2]].concat());
        assert!(r.members.contains_key(&d.key));
        assert!(!r.members.contains_key(&x.key));
        assert!(r.links.is_empty());
    }

    #[test]
    fn link_with_foreign_ticket_prefix_is_dropped() {
        let (f, x, c) = (id(), id(), id());
        let g = genesis(&f);
        let ax = admit(&f, &g, &x.key, "x");
        let t = tid(&f, "t");
        let lx = link_with(&x, &ax, &t, false);
        let xc = admit_via(&x, &lx, &c.key, "c", &t);
        let bare = link_with(&x, &xc, "t", false);
        let r = replay(&[g, ax, lx, xc, bare]);
        assert!(r.links.is_empty());
        assert!(!r.members.contains_key(&c.key));
    }

    #[test]
    fn new_link_retires_the_signers_old_link() {
        // X's second reusable link ends its first: an admission through the first that saw
        // the second drops, one before it stands; X's once link and F's link live on.
        let (f, x, c, d) = (id(), id(), id(), id());
        let g = genesis(&f);
        let ax = admit(&f, &g, &x.key, "x");
        let (t1, t2, t3, tf) = (tid(&x, "1"), tid(&x, "2"), tid(&x, "3"), tid(&f, "f"));
        let lf = link_with(&f, &ax, &tf, false);
        let l1 = link_with(&x, &lf, &t1, false);
        let lo = link_with(&x, &l1, &t3, true);
        let xc = admit_via(&x, &lo, &c.key, "c", &t1);
        let l2 = link_with(&x, &xc, &t2, false);
        let xd = admit_via(&x, &l2, &d.key, "d", &t1);
        let r = replay(&[g, ax, lf, l1, lo, xc, l2, xd]);
        assert!(r.members.contains_key(&c.key));
        assert!(!r.members.contains_key(&d.key));
        let mut live: Vec<&String> = r.links.keys().collect();
        live.sort();
        let mut want = vec![&t2, &t3, &tf];
        want.sort();
        assert_eq!(live, want);
    }

    #[test]
    fn inviter_may_admit_through_its_own_link_only() {
        let (f, x, y, c, d, e) = (id(), id(), id(), id(), id(), id());
        let g = genesis(&f);
        let ax = admit(&f, &g, &x.key, "x");
        let ay = admit(&f, &ax, &y.key, "y");
        let (tx, tf) = (tid(&x, "tx"), tid(&f, "tf"));
        let lx = link_with(&x, &ay, &tx, false);
        let lf = link_with(&f, &lx, &tf, false);
        let xc = admit_via(&x, &lf, &c.key, "c", &tx);
        let xd = admit_via(&x, &xc, &d.key, "d", &tf);
        let mut log = vec![g, ax, ay, lx, lf, xc, xd];
        let before = replay(&log);
        assert_eq!(before.links.len(), 2);
        assert_eq!(before.links[&tx].signer, x.key);
        assert_eq!(before.links[&tf].signer, f.key);
        let ry = member(&f, log.last().unwrap(), Kind::Remove, &y.key);
        let xe = admit_via(&x, &ry, &e.key, "e", &tx);
        log.extend([ry, xe]);
        let r = replay(&log);
        assert!(r.members.contains_key(&c.key));
        assert!(!r.members.contains_key(&d.key));
        assert!(!r.members.contains_key(&y.key));
        assert!(!r.members.contains_key(&e.key));
        assert!(r.links.is_empty());
    }

    #[test]
    fn member_open_link_and_elevated_or_cloud_admits_are_dropped() {
        let (f, x, c, k, m, z) = (id(), id(), id(), id(), id(), id());
        let g = genesis(&f);
        let ax = admit(&f, &g, &x.key, "x");
        // Member X: an open link is dropped, an ask link stands.
        let (open_id, ask_id) = (tid(&x, "open"), tid(&x, "ask"));
        let mut lo = link_with(&x, &ax, &open_id, false);
        lo.body["mode"] = json!("open");
        let lo = op(&x, &ax, Kind::Link, lo.body);
        let la = link_with(&x, &lo, &ask_id, false);
        // Through its own link X admits a computer, but neither a cloud member nor an admin.
        let xc = admit_via(&x, &la, &c.key, "c", &ask_id);
        let mut kb = admit_body(&k.key, "k");
        kb["kind"] = json!("cloud");
        kb["person_id"] = json!("ws:W2");
        kb["proof"] = json!(workspace_proof(&k.key, "W2", "k", &[], IAT));
        kb["ticket_id"] = json!(ask_id);
        let xk = op(&x, &xc, Kind::Admit, kb);
        let mut mb = admit_body(&m.key, "m");
        mb["role"] = json!("admin");
        mb["ticket_id"] = json!(ask_id);
        let xm = op(&x, &xk, Kind::Admit, mb.clone());
        // The founder may admit a cloud member (with its workspace proof), but not an admin.
        let mut zb = admit_body(&z.key, "z");
        zb["kind"] = json!("cloud");
        zb["person_id"] = json!("ws:W1");
        zb["proof"] = json!(workspace_proof(&z.key, "W1", "z", &[], IAT));
        let fz = op(&f, &xm, Kind::Admit, zb);
        let fm = op(&f, &fz, Kind::Admit, mb);
        let r = replay(&[g, ax, lo, la, xc, xk, xm, fz, fm]);
        assert!(!r.links.contains_key(&open_id));
        assert!(r.links.contains_key(&ask_id));
        assert!(r.members.contains_key(&c.key));
        assert!(!r.members.contains_key(&k.key));
        assert!(!r.members.contains_key(&m.key));
        assert_eq!(r.members[&z.key].kind, MemberKind::Cloud);
    }
}
