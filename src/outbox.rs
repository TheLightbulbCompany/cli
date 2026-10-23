//! The sender's outbox: mail for another member's session waits here until that member acks
//! it, sent again with backoff (1 s doubling to 60 s) for `TTL_S`, then `expired`
//! (`unreachable`). The receipt itself is the sender's `receipts` row (`mail`); a row here
//! outlives its first ack, as the binding of the envelope to the one member whose `Ack` and
//! `Receipt` frames may move it, until `KEEP_S` past a final state (or past any answer).

use crate::mail::{self, Prepared, Receipt, State};
use crate::net::frames::Frame;
use crate::roster::Member;
use crate::store::Store;
use crate::Result;

/// How long a message waits for its member.
pub const TTL_S: u64 = 86_400;
/// How long a row is kept after a final state, and `seen_ids` after arrival: past
/// `HELD_S`, so a hold answered on its last day still finds its binding.
const KEEP_S: u64 = mail::HELD_S + 86_400;

/// Keeps `p` for member `to`, due now: its receipt, `queued` on `to`'s computer.
pub fn enqueue(store: &Store, p: Prepared, to: &Member) -> Result<Receipt> {
    let frame = serde_json::to_string(&Frame::from(&p))?;
    let tx = store.conn().unchecked_transaction()?;
    mail::record_queued(store, &p)?;
    store.put_outbox(&p.message_id, &to.key, &frame, p.sent_at)?;
    tx.commit()?;
    let mut r = mail::receipt(store, &p.from, &p.request_id)?.ok_or("receipt missing")?;
    r.where_ = Some(to.computer.clone());
    Ok(r)
}

/// The frames to send at `now` to the `online` members, `(row id, member, frame)`: envelopes
/// still `queued`, hold notices and decisions created after `since` (`decision`), all due.
pub fn due(
    store: &Store,
    online: &[String],
    now: u64,
    since: u64,
) -> Result<Vec<(String, String, Frame)>> {
    let now = i64::try_from(now).unwrap_or(i64::MAX);
    let mut out = vec![];
    for (id, member, frame) in store.outbox_due(online, now, since as i64)? {
        out.push((id, member, serde_json::from_str(&frame)?));
    }
    Ok(out)
}

/// Envelope `id` went out at `now`: the next try is 1, 2, 4 … at most 60 s later.
pub fn sent(store: &Store, id: &str, now: u64) -> Result<()> {
    Ok(store.outbox_sent(id, now as i64)?)
}

/// `member`'s word on envelope `id` (with `limit`, the loop limit a refusal hit): its receipt
/// moves forward only (`State::later_than`; `refused` and `expired` final). `None` when the
/// envelope did not go to `member`: the one binding an `Ack` or `Receipt` is held to; else
/// whether the receipt moved (to `state`).
pub fn acked(
    store: &Store,
    member: &str,
    id: &str,
    state: State,
    reason: Option<&str>,
    limit: Option<&str>,
    now: u64,
) -> Result<Option<bool>> {
    let Some((from, request_id)) = store.outbox_sender(id, member)? else {
        return Ok(None);
    };
    let moved = mail::advance(store.conn(), &from, &request_id, state, reason, limit, now)?;
    Ok(Some(moved))
}

/// At `now`: mail queued `ttl` seconds becomes `expired` (`unreachable`); rows a week and a
/// day past a final state, or past any answer (`HELD_S + KEEP_S` old), and `seen_ids` that
/// old are dropped. The envelope ids that expired.
pub fn expire(store: &Store, now: u64, ttl: u64) -> Result<Vec<String>> {
    let tx = store.conn().unchecked_transaction()?;
    let mut expired = vec![];
    for (from, request_id, id) in store.outbox_stale(now.saturating_sub(ttl) as i64)? {
        let unreachable = Some("unreachable");
        if mail::advance(
            &tx,
            &from,
            &request_id,
            State::Expired,
            unreachable,
            None,
            now,
        )? {
            expired.push(id);
        }
    }
    let ago = |s: u64| now.saturating_sub(s) as i64;
    store.prune_mail(ago(KEEP_S), ago(mail::HELD_S + KEEP_S), ago(KEEP_S))?;
    tx.commit()?;
    Ok(expired)
}

/// The receipts owed to members: `(member, Receipt frame)`.
pub fn receipts(store: &Store) -> Result<Vec<(String, Frame)>> {
    mail::untold(store)
}

/// `member` was sent the receipt for `id` at `state`.
pub fn told(store: &Store, member: &str, id: &str, state: &str) -> Result<()> {
    Ok(store.set_told(member, id, state)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::Prep;
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
            workspace: None,
        }
    }

    fn state(s: &Store) -> (State, Option<String>) {
        let r = mail::receipt(s, &Key("a/codex:1".into()), "r1")
            .unwrap()
            .unwrap();
        (r.state, r.reason)
    }

    fn bound(s: &Store, id: &str, member: &str) -> bool {
        s.outbox_sender(id, member).unwrap().is_some()
    }

    fn queue(s: &Store, now: u64) -> String {
        let (from, to) = (Key("a/codex:1".into()), Key("b/codex:2".into()));
        let Prep::Go(p) = mail::prepare(s, &from, Some(&to), "hi", "r1", None, now).unwrap() else {
            panic!("refused");
        };
        let id = p.message_id.clone();
        let r = enqueue(s, p, &member("b")).unwrap();
        assert_eq!((r.state, r.where_.as_deref()), (State::Queued, Some("mbp")));
        id
    }

    fn due_n(s: &Store, now: u64) -> usize {
        due(s, &["b".into()], now, 0).unwrap().len()
    }

    #[test]
    fn acks_move_a_receipt_forward_only_and_only_from_its_member() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("state.db")).unwrap();
        let id = queue(&s, 100);
        assert_eq!(due_n(&s, 100), 1);
        assert!(
            due(&s, &["c".into()], 100, 0).unwrap().is_empty(),
            "only for the online"
        );
        sent(&s, &id, 100).unwrap();
        assert_eq!(due_n(&s, 100), 0, "next try 1 s later");
        assert_eq!(due_n(&s, 101), 1);
        sent(&s, &id, 101).unwrap();
        assert_eq!(due_n(&s, 102), 0, "then 2 s");
        assert_eq!(
            acked(&s, "c", &id, State::Read, None, None, 102).unwrap(),
            None
        );
        assert!(!bound(&s, &id, "c") && bound(&s, &id, "b"));
        for (st, want) in [
            (State::Held, State::Held),
            (State::Stored, State::Stored),
            (State::Held, State::Stored),
            (State::Read, State::Read),
            (State::Delivered, State::Read),
        ] {
            let moved = state(&s).0 != want;
            assert_eq!(
                acked(&s, "b", &id, st, None, None, 103).unwrap(),
                Some(moved)
            );
            assert_eq!(state(&s).0, want);
        }
        assert_eq!(due_n(&s, 1_000), 0, "acked: never sent again");
        // Kept as the binding a while past its final state, then dropped.
        expire(&s, 103 + KEEP_S, TTL_S).unwrap();
        assert!(bound(&s, &id, "b"));
        expire(&s, 104 + KEEP_S, TTL_S).unwrap();
        assert!(!bound(&s, &id, "b"));
    }

    #[test]
    fn a_row_left_at_delivered_is_dropped_once_no_answer_can_come() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("state.db")).unwrap();
        let id = queue(&s, 100);
        acked(&s, "b", &id, State::Delivered, None, None, 101).unwrap();
        let end = 100 + mail::HELD_S + KEEP_S;
        expire(&s, end, TTL_S).unwrap();
        assert!(bound(&s, &id, "b"));
        expire(&s, end + 1, TTL_S).unwrap();
        assert!(!bound(&s, &id, "b"));
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
        assert_eq!(due_n(&s, u64::MAX / 2), 0);
        // Final: a late ack changes nothing.
        acked(&s, "b", &id, State::Stored, None, None, 100 + TTL_S).unwrap();
        assert_eq!(state(&s).0, State::Expired);
    }
}
