//! Memory sync over a member connection (`Frame::MemoryHeads`, `Frame::Memory`), for a peer
//! whose `hello` said `memory`. Entries are verified by `memory::accept`, not here.

use super::frames::Frame;
use super::{Tx, PAGE_BYTES, PAGE_ENTRIES};
use crate::daemon::{ipc, Daemon};
use crate::memory::{self, Op, Scope};
use crate::Result;
use std::collections::BTreeMap;
use std::time::Duration;

/// How long a page waits for room in a link's send queue before the link is closed as stalled.
/// It waits with no lock held; links are served one after another, so this bounds the delay one
/// stalled peer costs the rest.
pub(super) const PAGE_WAIT: Duration = Duration::from_secs(5);

/// Whether `e` may go to a peer: a `me` fact never leaves this computer.
// ponytail: nothing proves two computers are one person without a sign-in, so `me` memory
// never syncs; compare roster `person_id`s to sync it between one person's computers.
fn sendable(e: &memory::Entry) -> bool {
    !matches!(
        e.op,
        Op::Remember {
            scope: Scope::Me,
            ..
        }
    )
}

/// Answers a peer's heads (`theirs`, empty for a push), or pushes to it: sends `tx` every
/// entry we hold past `theirs` and past what its link holds or has queued (`Tx::claim`), per
/// author in order, a page closing before the next entry would take it past `PAGE_BYTES`.
/// What the `me` filter kept back is not owed again. A failure is logged and closes the
/// link: what it claimed was never all queued.
pub(super) fn send_pages(d: &Daemon, tx: &Tx, theirs: &BTreeMap<String, u64>) {
    if !d.team.lock().unwrap().config.memory_sync {
        return;
    }
    if let Err(e) = pages(d, tx, theirs) {
        eprintln!("net: memory pages: {e}");
        tx.close();
    }
}

fn pages(d: &Daemon, tx: &Tx, theirs: &BTreeMap<String, u64>) -> Result<()> {
    let ours = memory::heads(&d.store.lock().unwrap())?;
    let from = tx.claim(theirs, &ours);
    // `"more":false` is the longer spelling, so this bounds every page's overhead.
    let base = serde_json::to_vec(&Frame::Memory {
        entries: vec![],
        more: false,
    })?
    .len();
    let page = |entries: Vec<memory::Entry>, more| -> Result<()> {
        tx.send_waiting((Frame::Memory { entries, more }, None), PAGE_WAIT)
            .map_err(|()| "the link closed".into())
    };
    for (author, mine) in &ours {
        let mine = *mine;
        let mut after = from.get(author).copied().unwrap_or(0);
        while mine > after {
            let batch = memory::since(&d.store.lock().unwrap(), author, after, PAGE_ENTRIES)?;
            let Some(last) = batch.last().map(|e| e.seq) else {
                break;
            };
            // A short batch is the end of the stream (seqs have gaps: never wait for one).
            let full = batch.len() == PAGE_ENTRIES;
            let (mut cur, mut size) = (vec![], base);
            for e in batch.into_iter().filter(sendable) {
                let n = serde_json::to_vec(&e)?.len() + 1;
                if !cur.is_empty() && size + n > PAGE_BYTES {
                    page(std::mem::take(&mut cur), true)?;
                    size = base;
                }
                size += n;
                cur.push(e);
            }
            if !cur.is_empty() {
                page(cur, full)?;
            }
            after = last;
            if !full {
                break;
            }
        }
    }
    Ok(())
}

/// Stores the entries of a `memory` frame from `short`; one that does not verify is dropped,
/// logged without its text.
pub(super) fn store(d: &Daemon, short: &str, entries: &[memory::Entry]) {
    if !d.team.lock().unwrap().config.memory_sync {
        return;
    }
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
    fn me_stays_home() {
        assert!(!sendable(&entry(Scope::Me)));
        assert!(sendable(&entry(Scope::Team)));
        let forget = memory::Entry {
            op: Op::Forget { target: "x".into() },
            ..entry(Scope::Me)
        };
        assert!(sendable(&forget));
    }
}
