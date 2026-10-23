//! Whether a peer's message may reach a session here. Task 9 fills it in from the policy
//! table; until then every envelope that passes `mail::receive`'s checks is accepted.

use crate::roster::Member;
use crate::store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Accept,
    Hold,
    Refuse,
}

/// The verdict on `peer`'s message for session `to`.
pub fn decide(_store: &Store, _peer: &Member, _to: &str) -> Verdict {
    Verdict::Accept
}
