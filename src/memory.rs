//! Team memory: facts a member's agents remember, as signed entries numbered per author
//! (`seq`). Unlike the roster there is no DAG: an entry stands on its author's signature alone.
//! `Forget` tombstones an entry (its id stays, its body goes); `facts` and `search` replay what
//! is held into current, contested and replaced values.

use crate::embed_model;
use crate::identity::{canonical, unsigned, verify, Identity};
use crate::roster::{self, MAX_ENTRY_BYTES};
use crate::store::Store;
use crate::Result;
use data_encoding::HEXLOWER;
use regex::Regex;
use rusqlite::{params, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Fact,
    Decision,
    Lesson,
    Preference,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Me,
    Team,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub harness: String,
    pub repo: Option<String>,
    pub session: Option<String>,
    pub on_behalf_of: Option<String>,
    /// The id of the `me` entry this `team` entry was copied from (`memory share`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_from: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Op {
    Remember {
        subject: String,
        fact: String,
        kind: Kind,
        scope: Scope,
        when: Option<i64>,
        source: Source,
        #[serde(default)]
        replaces: Vec<String>,
    },
    Forget {
        target: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub v: u32,
    /// Lowercase hex sha256 of `canonical(entry without id and sig)`.
    pub id: String,
    pub author: String,
    pub seq: u64,
    pub op: Op,
    pub at: i64,
    pub sig: String,
}

impl Entry {
    fn value(&self) -> Value {
        serde_json::to_value(self).expect("an entry serializes")
    }

    fn expected_id(&self) -> String {
        let mut v = self.value();
        v["id"] = "".into();
        HEXLOWER.encode(&Sha256::digest(unsigned(&v)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Current,
    /// Two current values for one thing: nobody's entry overwrites anybody's.
    Contested,
    Replaced,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Fact {
    pub id: String,
    pub subject: String,
    pub value: String,
    pub kind: Kind,
    pub scope: Scope,
    pub author: String,
    pub when: i64,
    pub status: Status,
    /// What this replaced, oldest first: (entry id, value, stamp).
    pub history: Vec<(String, String, i64)>,
}

/// Signs `op` as `id`'s next entry and stores it. `too_long` when it passes `MAX_ENTRY_BYTES`;
/// `secret:<pattern>` when any of its strings looks like a credential (`secret_in`; refused,
/// never scrubbed);
/// `no_subject` for a blank subject (replay would group it with every other one).
pub fn remember(store: &Store, id: &Identity, op: Op, now: i64) -> Result<Entry> {
    if let Op::Remember { subject, .. } = &op {
        if subject.trim().is_empty() {
            return Err("no_subject".into());
        }
    }
    if let Some(name) = secret_in(&op) {
        return Err(format!("secret:{name}").into());
    }
    // Immediate: a writer takes the reserved lock first, so a second connection waits out the
    // busy timeout instead of SQLite refusing the read-then-write upgrade at once.
    let tx = Transaction::new_unchecked(store.conn(), TransactionBehavior::Immediate)?;
    // Past what we hold and what a peer said it holds of ours (`saw`): a restored state.db
    // never reuses a number peers already hold.
    let seq: Option<u64> = tx.query_row(
        "SELECT MAX(seq) FROM (SELECT seq FROM memory_heads WHERE author = ?1
         UNION ALL SELECT seq FROM memory_seen WHERE author = ?1)",
        [&id.key],
        |r| r.get(0),
    )?;
    let mut e = Entry {
        v: 1,
        id: String::new(),
        author: id.key.clone(),
        // A stored number past the range (never from `saw`) refuses, never panics or wraps.
        seq: seq
            .unwrap_or(0)
            .checked_add(1)
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or("memory seq out of range")?,
        op,
        at: now,
        sig: String::new(),
    };
    e.id = e.expected_id();
    e.sig = id.sign(&e.value());
    if canonical(&e.value()).len() > MAX_ENTRY_BYTES {
        return Err("too_long".into());
    }
    insert(&tx, &e, now)?;
    tx.commit()?;
    Ok(e)
}

/// The secret pattern any string of a `remember` matches: its subject, fact and every source
/// field. `remember` and `accept` both check through this, so they check the same fields.
fn secret_in(op: &Op) -> Option<&'static str> {
    let Op::Remember {
        subject,
        fact,
        source,
        ..
    } = op
    else {
        return None;
    };
    // Every field named, so a new one does not compile until it is listed here.
    let Source {
        harness,
        repo,
        session,
        on_behalf_of,
        shared_from,
    } = source;
    [Some(subject), Some(fact), Some(harness)]
        .into_iter()
        .chain([repo, session, on_behalf_of, shared_from].map(Option::as_ref))
        .flatten()
        .find_map(|t| refuse_secret(t))
}

/// Stores `e` once it verifies: `Ok(true)` when stored, `Ok(false)` when already held. Refuses a
/// bad signature or id and an author the roster has not admitted. A `remember` whose subject or
/// fact looks like a credential (an older build did not refuse it) is held as a tombstone with
/// reason `refused:<pattern>` and never its text: its seq is held, so it is not asked for again.
pub fn accept(store: &Store, e: &Entry, now: i64) -> Result<bool> {
    if canonical(&e.value()).len() > MAX_ENTRY_BYTES {
        return Err("memory entry is too large".into());
    }
    if !verify(&e.author, &e.value(), &e.sig) || e.id != e.expected_id() {
        return Err("memory entry has a bad signature".into());
    }
    if !roster::replay(&roster::entries(store)?)
        .members
        .contains_key(&e.author)
    {
        return Err("memory entry is from a non-member".into());
    }
    let secret = secret_in(&e.op);
    let tx = Transaction::new_unchecked(store.conn(), TransactionBehavior::Immediate)?;
    let stored = match secret {
        None => insert(&tx, e, now)?,
        Some(name) => {
            tx.execute(
                "INSERT OR IGNORE INTO memory_entries
                 (id, author, seq, entry, received_at, tombstone, reason)
                 VALUES (?1, ?2, ?3, NULL, ?4, 1, ?5)",
                params![e.id, e.author, e.seq, now, format!("refused:{name}")],
            )? == 1
        }
    };
    tx.commit()?;
    Ok(stored)
}

/// The row for verified `e`; `Ok(false)` when its id is held. A `Forget` is an entry like any
/// other, and tombstones its target (any member's forget applies): the target's body is dropped
/// if held, and if it arrives later it is stored without its text; every `memory_last` block
/// is cleared.
fn insert(conn: &rusqlite::Connection, e: &Entry, now: i64) -> Result<bool> {
    let held = conn
        .prepare_cached("SELECT 1 FROM memory_entries WHERE id = ?1")?
        .exists([&e.id])?;
    if held {
        return Ok(false);
    }
    let tombstoned = matches!(e.op, Op::Remember { .. })
        && conn
            .prepare_cached("SELECT 1 FROM memory_tombstones WHERE target = ?1")?
            .exists([&e.id])?;
    let body = (!tombstoned)
        .then(|| serde_json::to_string(e))
        .transpose()?;
    conn.execute(
        "INSERT INTO memory_entries (id, author, seq, entry, received_at, tombstone)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![e.id, e.author, e.seq, body, now, tombstoned],
    )?;
    if let (Op::Remember { subject, fact, .. }, false) = (&e.op, tombstoned) {
        store_vec(conn, &e.id, subject, fact)?;
    }
    if let Op::Forget { target } = &e.op {
        conn.execute(
            "INSERT OR IGNORE INTO memory_tombstones (target) VALUES (?1)",
            [target],
        )?;
        conn.execute("DELETE FROM memory_vec WHERE id = ?1", [target])?;
        // A forget of a forget is accepted and does nothing.
        conn.execute(
            "UPDATE memory_entries SET entry = NULL, tombstone = 1
             WHERE id = ?1 AND json_extract(entry, '$.op.t') = 'remember'",
            [target],
        )?;
        // The blocks handed out may quote it: all go (the next session start makes its own).
        conn.execute("DELETE FROM memory_last", [])?;
    }
    Ok(true)
}

/// Embeds `subject: fact` into `memory_vec` for `id`; whether it did (false without a model).
fn store_vec(conn: &rusqlite::Connection, id: &str, subject: &str, fact: &str) -> Result<bool> {
    let Some(v) = embed_model::text(&format!("{subject}: {fact}")) else {
        return Ok(false);
    };
    let blob: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
    conn.execute(
        "INSERT OR REPLACE INTO memory_vec (id, v) VALUES (?1, ?2)",
        params![id, blob],
    )?;
    Ok(true)
}

/// A row with no vector yet: a memory entry by id, or a turn by rowid.
enum Missing {
    Entry(String),
    Turn(i64),
}

/// Up to `limit` held, live `remember`s with no vector, then turns with none (a refused turn is
/// never embedded), each with the text to embed.
fn missing(store: &Store, limit: usize) -> Result<Vec<(Missing, String)>> {
    let mut out = vec![];
    let mut stmt = store.conn().prepare(
        "SELECT id, entry FROM memory_entries WHERE tombstone = 0 AND entry IS NOT NULL
         AND json_extract(entry, '$.op.t') = 'remember'
         AND id NOT IN (SELECT id FROM memory_vec) ORDER BY rowid LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id, body) = row?;
        if let Op::Remember { subject, fact, .. } = serde_json::from_str::<Entry>(&body)?.op {
            out.push((Missing::Entry(id), format!("{subject}: {fact}")));
        }
    }
    let mut stmt = store.conn().prepare(
        "SELECT rowid, text FROM capture_turns WHERE rowid NOT IN (SELECT turn FROM turns_vec)
         AND substr(text, 1, length(?1)) != ?1 ORDER BY rowid LIMIT ?2",
    )?;
    let left = limit.saturating_sub(out.len()) as i64;
    let rows = stmt.query_map(params![crate::capture::REFUSED, left], |r| {
        Ok((Missing::Turn(r.get(0)?), r.get(1)?))
    })?;
    out.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
    Ok(out)
}

/// Writes one vector, unless its row went (forgotten or pruned) while it was embedded.
fn put_vec(conn: &rusqlite::Connection, row: &Missing, v: &[f32]) -> Result<usize> {
    let blob: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
    Ok(match row {
        Missing::Entry(id) => conn.execute(
            "INSERT OR REPLACE INTO memory_vec (id, v) SELECT ?1, ?2 WHERE EXISTS
             (SELECT 1 FROM memory_entries WHERE id = ?1 AND tombstone = 0 AND entry IS NOT NULL)",
            params![id, blob],
        )?,
        Missing::Turn(rowid) => conn.execute(
            "INSERT OR IGNORE INTO turns_vec (turn, v) SELECT ?1, ?2 WHERE EXISTS
             (SELECT 1 FROM capture_turns WHERE rowid = ?1)",
            params![rowid, blob],
        )?,
    })
}

/// Embeds up to `limit` held, live `remember`s, then turns, that have no vector; how many it
/// did. A no-op (0) without a model. `lock` gives the store for the read and for each write
/// only: the embedding (20-100 ms each) runs between, so no other op waits on it.
pub fn embed_missing_with<'a, S: std::ops::Deref<Target = Store> + 'a>(
    lock: impl Fn() -> S,
    limit: usize,
) -> Result<usize> {
    if !embed_model::installed() {
        return Ok(0);
    }
    let rows = missing(&lock(), limit)?;
    let mut n = 0;
    for (row, text) in rows {
        if let Some(v) = embed_model::text(&text) {
            n += put_vec(lock().conn(), &row, &v)?;
        }
    }
    Ok(n)
}

/// `embed_missing_with` on a store this caller holds.
pub fn embed_missing(store: &Store, limit: usize) -> Result<usize> {
    embed_missing_with(|| store, limit)
}

/// `(id, vector)` of every embedded entry that is neither tombstoned nor replaced.
pub fn vectors(store: &Store) -> Result<Vec<(String, Vec<f32>)>> {
    let current: std::collections::HashSet<String> = replay(store)?
        .into_iter()
        .filter(|l| l.fact.status != Status::Replaced)
        .map(|l| l.fact.id)
        .collect();
    let mut stmt = store
        .conn()
        .prepare("SELECT id, v FROM memory_vec ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, blob) = row?;
        if current.contains(&id) && blob.len() == embed_model::DIM * 4 {
            let v = blob
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            out.push((id, v));
        }
    }
    Ok(out)
}

/// Most of `key`'s entries a restore can lose: a peer's claim past our head by more is hostile
/// or corrupt.
const MAX_LOST: u64 = 1_000_000;

/// A peer holds `key`'s entries up to `seq` (its `MemoryHeads`): kept, so our next entry is
/// numbered past it. `Ok(false)`, nothing kept, for a claim past `MAX_LOST` beyond what we hold
/// or anywhere near `i64::MAX` (it would stop every later entry being numbered).
pub fn saw(store: &Store, key: &str, seq: u64) -> Result<bool> {
    let ours: Option<u64> = store.conn().query_row(
        "SELECT MAX(seq) FROM memory_entries WHERE author = ?1",
        [key],
        |r| r.get(0),
    )?;
    if seq >= i64::MAX as u64 / 2 || seq > ours.unwrap_or(0) + MAX_LOST {
        return Ok(false);
    }
    store.conn().execute(
        "INSERT INTO memory_seen (author, seq) VALUES (?1, ?2)
         ON CONFLICT(author) DO UPDATE SET seq = MAX(seq, excluded.seq)",
        params![key, seq],
    )?;
    Ok(true)
}

/// Each author's highest `seq` held.
pub fn heads(store: &Store) -> Result<BTreeMap<String, u64>> {
    let mut stmt = store
        .conn()
        .prepare("SELECT author, seq FROM memory_heads")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Up to `limit` of `author`'s entries after `after`, in order. A tombstoned entry's body is
/// gone, so it is skipped (its `Forget` is in the stream).
pub fn since(store: &Store, author: &str, after: u64, limit: usize) -> Result<Vec<Entry>> {
    let mut stmt = store.conn().prepare(
        "SELECT entry FROM memory_entries
         WHERE author = ?1 AND seq > ?2 AND entry IS NOT NULL ORDER BY seq LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![author, after, limit], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(serde_json::from_str(&row?)?);
    }
    Ok(out)
}

/// A live `remember`, with what replay needs of it.
struct Live {
    fact: Fact,
    repo: Option<String>,
    replaces: Vec<String>,
}

/// Every live (not tombstoned) `remember`, with its status. Replaced ones are included.
fn replay(store: &Store) -> Result<Vec<Live>> {
    let mut stmt = store
        .conn()
        .prepare("SELECT entry FROM memory_entries WHERE tombstone = 0 AND entry IS NOT NULL")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut live = Vec::new();
    for row in rows {
        let e: Entry = serde_json::from_str(&row?)?;
        if let Op::Remember {
            subject,
            fact,
            kind,
            scope,
            when,
            source,
            replaces,
        } = e.op
        {
            live.push(Live {
                fact: Fact {
                    id: e.id,
                    subject,
                    value: fact,
                    kind,
                    scope,
                    author: e.author,
                    when: when.unwrap_or(e.at),
                    status: Status::Current,
                    history: Vec::new(),
                },
                repo: source.repo,
                replaces,
            });
        }
    }
    live.sort_by(|a, b| (a.fact.when, &a.fact.id).cmp(&(b.fact.when, &b.fact.id)));

    let replaced: HashSet<&str> = live
        .iter()
        .flat_map(|l| l.replaces.iter().map(String::as_str))
        .collect();
    let by_id: HashMap<&str, usize> = live
        .iter()
        .enumerate()
        .map(|(i, l)| (l.fact.id.as_str(), i))
        .collect();

    // Unreplaced entries that share a scope and subject, or a replaced target, are one thing
    // with several current values: a `me` value never contests a `team` one.
    let mut group: Vec<usize> = (0..live.len()).collect();
    fn root(group: &mut [usize], mut i: usize) -> usize {
        while group[i] != i {
            group[i] = group[group[i]];
            i = group[i];
        }
        i
    }
    let (mut by_key, mut status) = (HashMap::new(), vec![Status::Replaced; live.len()]);
    for (i, l) in live.iter().enumerate() {
        if replaced.contains(l.fact.id.as_str()) {
            continue;
        }
        status[i] = Status::Current;
        let keys = std::iter::once(format!(
            "s:{:?}:{}",
            l.fact.scope,
            l.fact.subject.trim().to_lowercase()
        ))
        .chain(l.replaces.iter().map(|t| format!("t:{t}")));
        for k in keys {
            let j = *by_key.entry(k).or_insert(i);
            let (a, b) = (root(&mut group, i), root(&mut group, j));
            group[a] = b;
        }
    }
    let mut values: HashMap<usize, HashSet<&str>> = HashMap::new();
    let mut sizes: HashMap<usize, usize> = HashMap::new();
    for (i, l) in live
        .iter()
        .enumerate()
        .filter(|(i, _)| status[*i] != Status::Replaced)
    {
        let r = root(&mut group, i);
        values.entry(r).or_default().insert(&l.fact.value);
        *sizes.entry(r).or_default() += 1;
    }
    for (i, s) in status.iter_mut().enumerate() {
        if *s != Status::Replaced {
            let r = root(&mut group, i);
            if sizes[&r] > 1 && values[&r].len() > 1 {
                *s = Status::Contested;
            }
        }
    }

    // History: everything it replaced, transitively, oldest first.
    let histories: Vec<Vec<(String, String, i64)>> = live
        .iter()
        .map(|l| {
            let (mut seen, mut queue) = (HashSet::new(), VecDeque::from(l.replaces.clone()));
            while let Some(t) = queue.pop_front() {
                if let Some(&j) = by_id.get(t.as_str()).filter(|_| seen.insert(t.clone())) {
                    queue.extend(live[j].replaces.iter().cloned());
                }
            }
            let mut h: Vec<&Live> = seen.iter().map(|t| &live[by_id[t.as_str()]]).collect();
            h.sort_by(|a, b| (a.fact.when, &a.fact.id).cmp(&(b.fact.when, &b.fact.id)));
            h.iter()
                .map(|p| (p.fact.id.clone(), p.fact.value.clone(), p.fact.when))
                .collect()
        })
        .collect();
    for ((l, s), h) in live.iter_mut().zip(status).zip(histories) {
        l.fact.status = s;
        l.fact.history = h;
    }
    Ok(live)
}

/// What the team currently holds, newest first: the current and contested values, with what
/// each replaced. `repo` keeps entries whose `source.repo` is it (`same_repo`).
pub fn facts(store: &Store, scope: Option<Scope>, repo: Option<&str>) -> Result<Vec<Fact>> {
    let mut out: Vec<Fact> = replay(store)?
        .into_iter()
        .filter(|l| l.fact.status != Status::Replaced)
        .filter(|l| scope.is_none_or(|s| s == l.fact.scope))
        .filter(|l| repo.is_none_or(|r| l.repo.as_deref().is_some_and(|k| same_repo(k, r))))
        .map(|l| l.fact)
        .collect();
    out.reverse();
    Ok(out)
}

/// Whether stored repo key `key` is `asked`, ignoring case. When either is a bare `name` (an
/// entry from a build before the owner was in the key, an origin with no owner, or a name a
/// person typed), only the names are compared.
fn same_repo(key: &str, asked: &str) -> bool {
    let name = |s: &str| s.rsplit('/').next().unwrap_or_default().to_lowercase();
    key.eq_ignore_ascii_case(asked)
        || ((!key.contains('/') || !asked.contains('/')) && name(key) == name(asked))
}

/// The held entry whose id is `id` or the only one that starts with it (six hex digits or more).
pub fn entry(store: &Store, id: &str) -> Result<Option<Entry>> {
    if id.len() < 6 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let mut stmt = store.conn().prepare(
        "SELECT entry FROM memory_entries WHERE id LIKE ?1 || '%' AND entry IS NOT NULL LIMIT 2",
    )?;
    let rows: Vec<String> = stmt
        .query_map([id.to_lowercase()], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    match rows.as_slice() {
        [one] => Ok(Some(serde_json::from_str(one)?)),
        _ => Ok(None),
    }
}

/// The fact with entry id `id`, whatever its status (a replaced one included).
pub fn fact(store: &Store, id: &str) -> Result<Option<Fact>> {
    Ok(replay(store)?
        .into_iter()
        .map(|l| l.fact)
        .find(|f| f.id == id))
}

/// Every held entry that still has its body.
pub fn entries(store: &Store) -> Result<Vec<Entry>> {
    let mut stmt = store.conn().prepare(
        "SELECT entry FROM memory_entries WHERE entry IS NOT NULL ORDER BY received_at, id",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(serde_json::from_str(&row?)?);
    }
    Ok(out)
}

/// Up to `limit` rows after rowid `after`, in insertion order, as `(rowid, json)`: the entry with
/// `seq` set to the rowid, or `{seq, id, author, tombstone: true}` for a row without a body.
pub fn page_since(store: &Store, after: i64, limit: usize) -> Result<Vec<(i64, Value)>> {
    let mut stmt = store.conn().prepare(
        "SELECT rowid, id, author, entry FROM memory_entries WHERE rowid > ?1
         ORDER BY rowid LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![after, limit], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (seq, id, author, body) = row?;
        let mut v = match body {
            Some(b) => serde_json::from_str(&b)?,
            None => serde_json::json!({ "id": id, "author": author, "tombstone": true }),
        };
        v["seq"] = seq.into();
        out.push((seq, v));
    }
    Ok(out)
}

/// Words a question carries that say nothing about what is asked.
const STOPWORDS: [&str; 37] = [
    "how", "do", "does", "we", "i", "you", "the", "a", "an", "of", "to", "in", "on", "is", "are",
    "was", "were", "be", "it", "that", "this", "what", "when", "where", "which", "who", "why",
    "before", "after", "with", "for", "and", "or", "not", "our", "my", "your",
];

/// The best `limit` matches for `query` (FTS5 `bm25`, subject and fact, stemmed), as `facts`
/// shows them; a match that has since been replaced says so. Any word but a stopword may match,
/// a word of 3 or more characters as a prefix too; a query of stopwords only is matched as the
/// whole phrase.
/// The least cosine a vector match counts at (`ponytail:` one constant for the current model;
/// tune on real recalls, per-model when the model changes).
const COSINE_FLOOR: f32 = 0.25;

pub fn search(store: &Store, query: &str, limit: usize) -> Result<Vec<Fact>> {
    // Each word quoted (its own quotes dropped), so FTS5 syntax in a query is text.
    let words: Vec<String> = query
        .split_whitespace()
        .map(|w| w.replace('"', ""))
        .filter(|w| !w.is_empty())
        .collect();
    let q = words
        .iter()
        .filter(|w| !STOPWORDS.contains(&w.to_lowercase().as_str()))
        .map(|w| {
            let star = if w.chars().count() >= 3 { "*" } else { "" };
            format!("\"{w}\"{star}")
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    if words.is_empty() {
        return Ok(Vec::new());
    }
    let q = if q.is_empty() {
        format!("\"{}\"", words.join(" "))
    } else {
        q
    };
    let mut stmt = store.conn().prepare(
        "SELECT m.id FROM memory_fts JOIN memory_entries m ON m.rowid = memory_fts.rowid
         WHERE memory_fts MATCH ?1 ORDER BY bm25(memory_fts) LIMIT ?2",
    )?;
    let ids: Vec<String> = stmt
        .query_map(params![q, limit], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    // With a model, fuse the bm25 order with the cosine order of the query's embedding.
    let ids = match embed_model::text(query) {
        Some(qv) => {
            let mut scored: Vec<(f32, String)> = vectors(store)?
                .into_iter()
                .map(|(id, v)| (embed_model::cosine(&qv, &v), id))
                .collect();
            scored.sort_by(|a, b| b.0.total_cmp(&a.0));
            // Below the floor a candidate shares no meaning with the question: without it a
            // short or nonsense question fills the page with whatever is nearest.
            let cos: Vec<String> = scored
                .into_iter()
                .filter(|s| s.0 >= COSINE_FLOOR)
                .take(50)
                .map(|s| s.1)
                .collect();
            rrf(&[ids, cos], limit)
        }
        None => ids,
    };
    let mut all: HashMap<String, Fact> = replay(store)?
        .into_iter()
        .map(|l| (l.fact.id.clone(), l.fact))
        .collect();
    Ok(ids.iter().filter_map(|id| all.remove(id)).collect())
}

/// Reciprocal rank fusion (k = 60) of ranked id lists, best first, cut to `limit`.
fn rrf(lists: &[Vec<String>], limit: usize) -> Vec<String> {
    let mut score: HashMap<&str, f64> = HashMap::new();
    for list in lists {
        for (rank, id) in list.iter().enumerate() {
            *score.entry(id).or_default() += 1.0 / (60.0 + rank as f64 + 1.0);
        }
    }
    let mut out: Vec<_> = score.into_iter().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
    out.into_iter()
        .take(limit)
        .map(|(id, _)| id.to_string())
        .collect()
}

static SECRETS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        (
            "credential",
            r"(?i)(api[_-]?key|secret|token|password)\s*[:=]\s*\S{8,}",
        ),
        ("private_key", r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
        ("aws_key", r"AKIA[0-9A-Z]{16}"),
        ("api_key", r"sk-[A-Za-z0-9_-]{20,}"),
        (
            "vendor_key",
            r"\b(?:[sr]k_(?:live|test)_[A-Za-z0-9]{16,}|gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{20,}|xox[abprs]-[A-Za-z0-9-]{10,}|AIza[0-9A-Za-z_-]{35}|glpat-[A-Za-z0-9_-]{20})",
        ),
        ("url_credentials", r"https?://[^\s/]+:[^\s@]+@"),
        ("jwt", r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}"),
    ]
    .map(|(n, p)| (n, Regex::new(p).expect("a valid pattern")))
    .into()
});

/// The name of the secret pattern `text` matches, if any. Refuse, never scrub.
pub fn refuse_secret(text: &str) -> Option<&'static str> {
    SECRETS
        .iter()
        .find(|(_, re)| re.is_match(text))
        .map(|(n, _)| *n)
}

/// What an agent reads: `Contested:` lines, then `Current:`, then `Lessons:` (replaced facts
/// left out), whole lines only, within `budget` chars. `names` maps a member key to the name
/// shown (the short key when it is not there).
/// The heading and the instruction line a session-start block carries around the page.
pub const BLOCK_HEADING: &str = "Memory (team and yours):";
pub const BLOCK_INSTRUCTION: &str = "Call `recall` when a task needs something a teammate may know; call `remember` when you finish a task or are told to remember. Personal unless the person says to share.";

/// What a session is handed at its start; empty when there is no page.
pub fn block(page: &str) -> String {
    if page.is_empty() {
        return String::new();
    }
    format!("{BLOCK_HEADING}\n{page}\n{BLOCK_INSTRUCTION}")
}

pub fn page(
    store: &Store,
    facts: &[Fact],
    names: &HashMap<String, String>,
    budget: usize,
) -> String {
    let who = |key: &str| {
        names
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.chars().take(12).collect())
    };
    let author_of = |id: &str| -> String {
        store
            .conn()
            .query_row(
                "SELECT author FROM memory_entries WHERE id = ?1",
                [id],
                |r| r.get::<_, String>(0),
            )
            .map_or_else(|_| "?".into(), |k| who(&k))
    };
    let by = |f: &Fact| {
        format!(
            "{} ({}, {})",
            f.value,
            who(&f.author),
            crate::date::day(f.when)
        )
    };
    let (mut contested, mut current, mut lessons) = (Vec::new(), Vec::new(), Vec::new());
    let mut groups: Vec<((Scope, String), Vec<String>)> = Vec::new();
    for f in facts {
        match (f.status, f.kind) {
            (Status::Replaced, _) => {}
            (Status::Contested, _) => {
                let key = (f.scope, f.subject.trim().to_lowercase());
                match groups.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, v)) => v.push(by(f)),
                    None => groups.push((key, vec![format!("{}: {}", f.subject, by(f))])),
                }
            }
            (_, kind) => {
                let was = f.history.last().map_or(String::new(), |(id, v, when)| {
                    format!("; was {v}, {}, {}", author_of(id), crate::date::day(*when))
                });
                let line = format!(
                    "{}: {} ({}, {}{was})",
                    f.subject,
                    f.value,
                    who(&f.author),
                    crate::date::day(f.when)
                );
                if kind == Kind::Lesson {
                    &mut lessons
                } else {
                    &mut current
                }
                .push(line);
            }
        }
    }
    contested.extend(groups.into_iter().map(|(_, v)| v.join(" vs ")));
    // A line that does not fit is left out and the next one tried, so one long line does not
    // hide the lines and sections after it. A section's heading comes with its first line kept.
    let mut out = String::new();
    for (head, lines) in [
        ("Contested:", contested),
        ("Current:", current),
        ("Lessons:", lessons),
    ] {
        let mut headed = false;
        for l in &lines {
            let add = if headed {
                format!("{l}\n")
            } else {
                format!("{head}\n{l}\n")
            };
            if out.chars().count() + add.chars().count() <= budget {
                out += &add;
                headed = true;
            }
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod rrf_tests {
    use super::rrf;

    #[test]
    fn rrf_puts_an_entry_ranked_by_both_arms_first() {
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let out = rrf(&[l(&["a", "b", "c"]), l(&["d", "b", "e"])], 3);
        assert_eq!(out, l(&["b", "a", "d"]));
    }
}
