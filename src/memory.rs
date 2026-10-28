//! Team memory: facts a member's agents remember, as signed entries numbered per author
//! (`seq`). Unlike the roster there is no DAG: an entry stands on its author's signature alone.
//! `Forget` tombstones an entry (its id stays, its body goes); `facts` and `search` replay what
//! is held into current, contested and replaced values.

use crate::identity::{canonical, unsigned, verify, Identity};
use crate::roster::{self, MAX_ENTRY_BYTES};
use crate::store::Store;
use crate::Result;
use data_encoding::HEXLOWER;
use regex::Regex;
use rusqlite::params;
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
/// `secret:<pattern>` when subject or fact looks like a credential (refused, never scrubbed);
/// `no_subject` for a blank subject (replay would group it with every other one).
pub fn remember(store: &Store, id: &Identity, op: Op, now: i64) -> Result<Entry> {
    if let Op::Remember { subject, fact, .. } = &op {
        if subject.trim().is_empty() {
            return Err("no_subject".into());
        }
        if let Some(name) = refuse_secret(subject).or_else(|| refuse_secret(fact)) {
            return Err(format!("secret:{name}").into());
        }
    }
    let tx = store.conn().unchecked_transaction()?;
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
        seq: seq.unwrap_or(0) + 1,
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

/// Stores `e` once it verifies: `Ok(true)` when stored, `Ok(false)` when already held. Refuses a
/// bad signature or id and an author the roster has not admitted.
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
    let tx = store.conn().unchecked_transaction()?;
    let stored = insert(&tx, e, now)?;
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
    if let Op::Forget { target } = &e.op {
        conn.execute(
            "INSERT OR IGNORE INTO memory_tombstones (target) VALUES (?1)",
            [target],
        )?;
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

/// A peer holds `key`'s entries up to `seq` (its `MemoryHeads`): kept, so our next entry is
/// numbered past it.
pub fn saw(store: &Store, key: &str, seq: u64) -> Result<()> {
    store.conn().execute(
        "INSERT INTO memory_seen (author, seq) VALUES (?1, ?2)
         ON CONFLICT(author) DO UPDATE SET seq = MAX(seq, excluded.seq)",
        params![key, seq],
    )?;
    Ok(())
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

    // Unreplaced entries that share a subject, or a replaced target, are one thing with
    // several current values.
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
        let keys = std::iter::once(format!("s:{}", l.fact.subject.trim().to_lowercase()))
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
/// each replaced. `repo` keeps entries whose `source.repo` is it.
pub fn facts(store: &Store, scope: Option<Scope>, repo: Option<&str>) -> Result<Vec<Fact>> {
    let mut out: Vec<Fact> = replay(store)?
        .into_iter()
        .filter(|l| l.fact.status != Status::Replaced)
        .filter(|l| scope.is_none_or(|s| s == l.fact.scope))
        .filter(|l| repo.is_none_or(|r| l.repo.as_deref() == Some(r)))
        .map(|l| l.fact)
        .collect();
    out.reverse();
    Ok(out)
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
    let mut all: HashMap<String, Fact> = replay(store)?
        .into_iter()
        .map(|l| (l.fact.id.clone(), l.fact))
        .collect();
    Ok(ids.iter().filter_map(|id| all.remove(id)).collect())
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
/// left out), cut on a line boundary at `budget` chars. `names` maps a member key to the name
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
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for f in facts {
        match (f.status, f.kind) {
            (Status::Replaced, _) => {}
            (Status::Contested, _) => {
                let key = f.subject.trim().to_lowercase();
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
    let mut out = String::new();
    'sections: for (head, lines) in [
        ("Contested:", contested),
        ("Current:", current),
        ("Lessons:", lessons),
    ] {
        for (i, l) in lines.iter().enumerate() {
            let add = if i == 0 {
                format!("{head}\n{l}\n")
            } else {
                format!("{l}\n")
            };
            if out.chars().count() + add.chars().count() > budget {
                break 'sections;
            }
            out += &add;
        }
    }
    out.trim_end().to_string()
}
