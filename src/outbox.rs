//! The sender's outbox: mail for another member's session waits here until that member acks
//! it, sent again with backoff (1 s doubling to 60 s) for `TTL_S`, then `expired`
//! (`unreachable`). The receipt itself is the sender's `receipts` row (`mail`); a row here
//! outlives its first ack, as the binding of the envelope to the one member whose `Ack` and
//! `Receipt` frames may move it, until `KEEP_S` past a terminal state.

use crate::mail::{self, Prepared, Receipt, State};
use crate::net::frames::Frame;
use crate::roster::Member;
use crate::store::Store;
use crate::Result;
use rusqlite::{params, OptionalExtension};

/// How long a message waits for its member.
pub const TTL_S: u64 = 86_400;
/// How long a row is kept after a terminal state, and `seen_ids` after arrival: past
/// `HELD_S`, so a hold answered on its last day still finds its binding.
const KEEP_S: u64 = mail::HELD_S + 86_400;

/// The rows of messages still `queued`, by receipt.
const QUEUED: &str = "FROM outbox o JOIN messages m ON m.id = o.message_id \
     JOIN receipts r ON r.from_key = m.from_key AND r.request_id = m.request_id";

/// `p` as the wire carries it.
pub fn envelope(p: &Prepared) -> Frame {
    Frame::Envelope {
        id: p.message_id.clone(),
        request_id: p.request_id.clone(),
        kind: p.kind.clone(),
        from: p.from.0.clone(),
        to: p.to.0.clone(),
        reply_to: p.reply_to.clone(),
        thread: Some(p.thread.clone()),
        hops: u32::try_from(p.hops).unwrap_or(u32::MAX),
        body: p.text.clone(),
        files: vec![],
        sent_at: p.sent_at as i64,
    }
}

/// Keeps `p` for member `to`, due now: its receipt, `queued` on `to`'s computer.
pub fn enqueue(store: &Store, p: Prepared, to: &Member) -> Result<Receipt> {
    let frame = serde_json::to_string(&envelope(&p))?;
    let tx = store.conn().unchecked_transaction()?;
    mail::record(store, &p, State::Queued, None)?;
    tx.execute(
        "INSERT INTO outbox VALUES (?1, ?2, ?3, 0, ?4, ?4)",
        params![p.message_id, to.key, frame, p.sent_at as i64],
    )?;
    tx.commit()?;
    let mut r = mail::receipt(store, &p.from, &p.request_id)?.ok_or("receipt missing")?;
    r.where_ = Some(to.computer.clone());
    Ok(r)
}

/// The envelopes to send at `now`, `(member, frame)`: still `queued`, and due.
pub fn due(store: &Store, now: u64) -> Result<Vec<(String, Frame)>> {
    let mut stmt = store.conn().prepare(&format!(
        "SELECT o.to_member, o.frame {QUEUED} WHERE r.state = 'queued' AND o.next_at <= ?1"
    ))?;
    let rows = stmt.query_map([i64::try_from(now).unwrap_or(i64::MAX)], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = vec![];
    for row in rows {
        let (member, frame) = row?;
        out.push((member, serde_json::from_str(&frame)?));
    }
    Ok(out)
}

/// Envelope `id` went out at `now`: the next try is 1, 2, 4 … at most 60 s later.
pub fn sent(store: &Store, id: &str, now: u64) -> Result<()> {
    store.conn().execute(
        "UPDATE outbox SET attempts = attempts + 1, next_at = ?2 + MIN(60, 1 << MIN(attempts, 6)) \
         WHERE message_id = ?1",
        params![id, now as i64],
    )?;
    Ok(())
}

/// Whether envelope `id` went to `member`: the one binding an `Ack` or `Receipt` is held to.
pub fn bound(store: &Store, id: &str, member: &str) -> Result<bool> {
    Ok(store
        .conn()
        .prepare("SELECT 1 FROM outbox WHERE message_id = ?1 AND to_member = ?2")?
        .exists([id, member])?)
}

/// `member`'s word on envelope `id`: its receipt moves forward only, by rank
/// (`queued < held < stored < delivered < read`; `refused` and `expired` final). `false` when
/// the envelope did not go to `member`.
pub fn acked(
    store: &Store,
    member: &str,
    id: &str,
    state: State,
    reason: Option<&str>,
    now: u64,
) -> Result<bool> {
    let row: Option<(String, String)> = store
        .conn()
        .query_row(
            "SELECT m.from_key, m.request_id FROM outbox o JOIN messages m ON m.id = o.message_id \
             WHERE o.message_id = ?1 AND o.to_member = ?2",
            [id, member],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((from, request_id)) = row else {
        return Ok(false);
    };
    mail::advance(store.conn(), &from, &request_id, state, reason, now)?;
    Ok(true)
}

/// At `now`: mail queued `ttl` seconds becomes `expired` (`unreachable`); rows a week past a
/// terminal state, `seen_ids` a week old and the receipts owed for them are dropped.
pub fn expire(store: &Store, now: u64, ttl: u64) -> Result<()> {
    let conn = store.conn();
    let tx = conn.unchecked_transaction()?;
    let old: Vec<(String, String)> = {
        let mut stmt = tx.prepare(&format!(
            "SELECT m.from_key, m.request_id {QUEUED} WHERE r.state = 'queued' \
             AND o.created_at + ?2 <= ?1"
        ))?;
        let rows = stmt.query_map(params![now as i64, ttl as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (from, request_id) in old {
        mail::advance(
            &tx,
            &from,
            &request_id,
            State::Expired,
            Some("unreachable"),
            now,
        )?;
    }
    let before = now.saturating_sub(KEEP_S) as i64;
    tx.execute(
        &format!(
            "DELETE FROM outbox WHERE message_id IN (SELECT o.message_id {QUEUED} \
             WHERE r.state IN ('read', 'refused', 'expired') AND r.updated_at < ?1)"
        ),
        [before],
    )?;
    // Whatever its state (a receipt may be lost on the way): gone once no answer can come.
    tx.execute(
        "DELETE FROM outbox WHERE created_at < ?1",
        [now.saturating_sub(mail::HELD_S + KEEP_S) as i64],
    )?;
    tx.execute("DELETE FROM seen_ids WHERE seen_at < ?1", [before])?;
    tx.execute(
        "DELETE FROM receipt_out WHERE NOT EXISTS (SELECT 1 FROM seen_ids s \
         WHERE s.sender = receipt_out.member AND s.envelope_id = receipt_out.envelope_id)",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

/// The receipts owed to members: `(member, Receipt frame)`.
pub fn receipts(store: &Store) -> Result<Vec<(String, Frame)>> {
    let mut stmt = store
        .conn()
        .prepare("SELECT member, envelope_id, state, reason FROM receipt_out")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get(0)?,
            Frame::Receipt {
                id: r.get(1)?,
                state: r.get(2)?,
                reason: r.get(3)?,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// `member` was sent the receipt for `id` at `state`.
pub fn told(store: &Store, member: &str, id: &str, state: &str) -> Result<()> {
    store.conn().execute(
        "DELETE FROM receipt_out WHERE member = ?1 AND envelope_id = ?2 AND state = ?3",
        [member, id, state],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roster::{MemberKind, Role};
    use crate::sessions::Key;

    fn member(key: &str) -> Member {
        Member {
            key: key.into(),
            short: key.into(),
            name: "bob".into(),
            computer: "mbp".into(),
            person_id: key.into(),
            role: Role::Member,
            kind: MemberKind::Computer,
            admitted_at: 0,
            proof: None,
        }
    }

    fn state(s: &Store) -> (State, Option<String>) {
        let r = mail::receipt(s, &Key("a/codex:1".into()), "r1")
            .unwrap()
            .unwrap();
        (r.state, r.reason)
    }

    fn queue(s: &Store, now: u64) -> String {
        let (from, to) = (Key("a/codex:1".into()), Key("b/codex:2".into()));
        let p = mail::prepare(s, &from, Some(&to), "hi", "r1", None, now)
            .unwrap()
            .unwrap();
        let id = p.message_id.clone();
        let r = enqueue(s, p, &member("b")).unwrap();
        assert_eq!((r.state, r.where_.as_deref()), (State::Queued, Some("mbp")));
        id
    }

    #[test]
    fn acks_move_a_receipt_forward_only_and_only_from_its_member() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("state.db")).unwrap();
        let id = queue(&s, 100);
        assert_eq!(due(&s, 100).unwrap().len(), 1);
        sent(&s, &id, 100).unwrap();
        assert!(due(&s, 100).unwrap().is_empty(), "next try 1 s later");
        assert_eq!(due(&s, 101).unwrap().len(), 1);
        sent(&s, &id, 101).unwrap();
        assert!(due(&s, 102).unwrap().is_empty(), "then 2 s");
        assert!(!acked(&s, "c", &id, State::Read, None, 102).unwrap());
        assert!(!bound(&s, &id, "c").unwrap() && bound(&s, &id, "b").unwrap());
        for (st, want) in [
            (State::Held, State::Held),
            (State::Stored, State::Stored),
            (State::Held, State::Stored),
            (State::Read, State::Read),
            (State::Delivered, State::Read),
        ] {
            assert!(acked(&s, "b", &id, st, None, 103).unwrap());
            assert_eq!(state(&s).0, want);
        }
        assert!(
            due(&s, 1_000).unwrap().is_empty(),
            "acked: never sent again"
        );
        // Kept as the binding a week past its final state, then dropped.
        expire(&s, 103 + KEEP_S, TTL_S).unwrap();
        assert!(bound(&s, &id, "b").unwrap());
        expire(&s, 104 + KEEP_S, TTL_S).unwrap();
        assert!(!bound(&s, &id, "b").unwrap());
    }

    #[test]
    fn a_row_left_at_delivered_is_dropped_once_no_answer_can_come() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("state.db")).unwrap();
        let id = queue(&s, 100);
        acked(&s, "b", &id, State::Delivered, None, 101).unwrap();
        let end = 100 + mail::HELD_S + KEEP_S;
        expire(&s, end, TTL_S).unwrap();
        assert!(bound(&s, &id, "b").unwrap());
        expire(&s, end + 1, TTL_S).unwrap();
        assert!(!bound(&s, &id, "b").unwrap());
    }

    #[test]
    fn a_message_no_one_acks_expires_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("state.db")).unwrap();
        let id = queue(&s, 100);
        expire(&s, 100 + TTL_S - 1, TTL_S).unwrap();
        assert_eq!(state(&s).0, State::Queued);
        expire(&s, 100 + TTL_S, TTL_S).unwrap();
        assert_eq!(state(&s), (State::Expired, Some("unreachable".into())));
        assert!(due(&s, u64::MAX / 2).unwrap().is_empty());
        // Terminal: a late ack changes nothing.
        acked(&s, "b", &id, State::Stored, None, 100 + TTL_S).unwrap();
        assert_eq!(state(&s).0, State::Expired);
    }
}
