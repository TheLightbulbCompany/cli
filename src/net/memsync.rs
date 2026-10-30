//! Memory sync over a member connection (`Frame::MemoryHeads`, `Frame::Memory`), for a peer
//! whose `hello` said `memory`. Entries are verified by `memory::accept`, not here.

use super::frames::Frame;
use super::{Tx, PAGE_BYTES, PAGE_ENTRIES};
use crate::daemon::{ipc, Daemon};
use crate::memory::{self, Op, Scope};
use crate::proof;
use crate::Result;
use std::collections::{BTreeMap, HashSet};

/// Whether `e` may go to a peer: a `me` fact only to a computer of the same person.
fn sendable(e: &memory::Entry, same_person: bool) -> bool {
    same_person
        || !matches!(
            e.op,
            Op::Remember {
                scope: Scope::Me,
                ..
            }
        )
}

/// Whether member `key` is the same person as this computer: both signed in, and `key`'s
/// verified `hello` proof names the same `sub` as our own sign-in. Else a `me` entry stays home.
pub(super) fn same_person(d: &Daemon, key: &str) -> bool {
    let now = ipc::now() as i64;
    let sub = |token: &str, k: &str| proof::verify(token, k, now).ok().map(|p| p.sub);
    let mine = crate::login::stored_proof(&d.dir).and_then(|t| sub(&t, &d.identity.key));
    let theirs = d.proofs.lock().unwrap().get(key).and_then(|t| sub(t, key));
    mine.is_some() && mine == theirs
}

/// The authors whose entries do not go to member `key`: a workspace computer's (one with a
/// doorbell in the roster, which only a workspace's cloud member publishes, `embed::publish`),
/// unless this is that computer and its workspace admits `key` by the gate its mail uses
/// (`proof::admitted` over the declared `members`; none declared: everyone, as for mail).
/// Another member cannot tell who is in that workspace, so it never relays them.
/// ponytail: a link holds what was kept back as sent, so a computer added to the workspace gets
/// the older entries at its next connection, not on the live one.
pub(super) fn withheld(d: &Daemon, key: &str) -> HashSet<String> {
    let me = &d.identity.key;
    let mut out: HashSet<String> = d
        .team
        .lock()
        .unwrap()
        .roster
        .wakes
        .keys()
        .cloned()
        .collect();
    out.remove(me);
    if let Some(c) = &d.cloud {
        let people = c.declared().members;
        if people.is_some_and(|p| !proof::admitted(d, &p).contains(key)) {
            out.insert(me.clone());
        }
    }
    out
}

/// Answers member `key`'s heads, or pushes to it past what it is known to hold: sends `tx` every
/// entry we hold past `theirs` (not a `withheld` author's), per author in order, a page closing before the next entry
/// would take it past `PAGE_BYTES`. Returns our heads, which `theirs` now covers (what a
/// `me` filter kept back is not owed again).
pub(super) fn send_pages(
    d: &Daemon,
    tx: &Tx,
    theirs: &BTreeMap<String, u64>,
    key: &str,
) -> Result<BTreeMap<String, u64>> {
    let same_person = same_person(d, key);
    let withheld = withheld(d, key);
    let ours = memory::heads(&d.store.lock().unwrap())?;
    // `"more":false` is the longer spelling, so this bounds every page's overhead.
    let base = serde_json::to_vec(&Frame::Memory {
        entries: vec![],
        more: false,
    })?
    .len();
    let page = |entries: Vec<memory::Entry>, more| {
        let _ = tx.send((Frame::Memory { entries, more }, None));
    };
    for (author, mine) in ours.iter().filter(|(a, _)| !withheld.contains(*a)) {
        let mine = *mine;
        let mut after = theirs.get(author).copied().unwrap_or(0);
        while mine > after {
            let batch = memory::since(&d.store.lock().unwrap(), author, after, PAGE_ENTRIES)?;
            let Some(last) = batch.last().map(|e| e.seq) else {
                break;
            };
            // A short batch is the end of the stream (seqs have gaps: never wait for one).
            let full = batch.len() == PAGE_ENTRIES;
            let (mut cur, mut size) = (vec![], base);
            for e in batch.into_iter().filter(|e| sendable(e, same_person)) {
                let n = serde_json::to_vec(&e)?.len() + 1;
                if !cur.is_empty() && size + n > PAGE_BYTES {
                    page(std::mem::take(&mut cur), true);
                    size = base;
                }
                size += n;
                cur.push(e);
            }
            if !cur.is_empty() {
                page(cur, full);
            }
            after = last;
            if !full {
                break;
            }
        }
    }
    Ok(ours)
}

/// Stores the entries of a `memory` frame from `short`; one that does not verify is dropped,
/// logged without its text.
pub(super) fn store(d: &Daemon, short: &str, entries: &[memory::Entry]) {
    let store = d.store.lock().unwrap();
    for e in entries {
        if let Err(err) = memory::accept(&store, e, ipc::now() as i64) {
            eprintln!("net: dropped a memory entry from {short}: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Kind, Source};

    fn entry(scope: Scope) -> memory::Entry {
        memory::Entry {
            v: 1,
            id: String::new(),
            author: String::new(),
            seq: 1,
            op: Op::Remember {
                subject: "s".into(),
                fact: "f".into(),
                kind: Kind::Fact,
                scope,
                when: None,
                source: Source {
                    harness: "t".into(),
                    repo: None,
                    session: None,
                    on_behalf_of: None,
                    shared_from: None,
                },
                replaces: vec![],
            },
            at: 0,
            sig: String::new(),
        }
    }

    #[test]
    fn me_goes_only_to_the_same_person() {
        assert!(sendable(&entry(Scope::Me), true));
        assert!(!sendable(&entry(Scope::Me), false));
        assert!(sendable(&entry(Scope::Team), false));
        let forget = memory::Entry {
            op: Op::Forget { target: "x".into() },
            ..entry(Scope::Me)
        };
        assert!(sendable(&forget, false));
    }
}
