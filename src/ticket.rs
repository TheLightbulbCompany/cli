//! Invite tickets: a signed, base64url JSON naming a roster `link`, carried in the join line.

use crate::identity::{self, canonical, Identity};
use crate::roster::{Role, Roster};
use crate::Result;
use data_encoding::{BASE64URL_NOPAD, HEXLOWER};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const WEB: &str = "https://thelightbulb.company/j/#";
const APP: &str = "lightbulb://join/";
const LINE: &str = "Join Lightbulb Connect: ";
/// A reusable (or open) link lasts 7 days; a once link 24 hours.
const REUSABLE_S: i64 = 7 * 86_400;
const ONCE_S: i64 = 86_400;
const MAX_MEMBERS: usize = 6;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ticket {
    pub v: u32,
    pub ticket_id: String,
    pub team_id: String,
    pub secret: String,
    /// `ask` or `open`.
    pub mode: String,
    pub inviter: String,
    pub relay_hint: String,
    /// The inviter's direct addresses when it signed (`ip:port`, at most `net::MAX_ADDRS`):
    /// what a joiner dials with no relay and no mDNS. Absent from older tickets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addrs: Vec<String>,
    pub members: Vec<String>,
    pub expires_at: i64,
    pub sig: String,
}

/// Hex sha256 of a ticket secret: what a once link stores.
pub fn hash(secret: &str) -> String {
    HEXLOWER.encode(&Sha256::digest(secret.as_bytes()))
}

/// A new ticket from `id` for a new link, and, for a `once` one, its secret's hash (what the
/// link entry carries in place of the secret). `relay_hint` is the first relay in effect
/// (`""` for none): a joiner dials through it before it knows its own list; `addrs` this
/// computer's direct addresses (`Peers::hints`).
pub fn mint(
    id: &Identity,
    roster: &Roster,
    mode: &str,
    once: bool,
    now: i64,
    hints: (String, Vec<String>),
) -> (Ticket, Option<String>) {
    let secret = BASE64URL_NOPAD.encode(&rand::random::<[u8; 32]>());
    let ttl = if once { ONCE_S } else { REUSABLE_S };
    let hashed = once.then(|| hash(&secret));
    let t = sign(
        id,
        roster,
        format!("{}.{}", id.short, ulid::Ulid::generate()),
        secret,
        mode,
        now + ttl,
        hints,
    );
    (t, hashed)
}

/// The ticket `id` signs for its link `ticket_id`: the same bytes each time for the same link
/// and roster (Ed25519 is deterministic), so a reusable link reprints unchanged.
pub fn sign(
    id: &Identity,
    roster: &Roster,
    ticket_id: String,
    secret: String,
    mode: &str,
    expires_at: i64,
    (relay_hint, addrs): (String, Vec<String>),
) -> Ticket {
    // The inviter first, then the founder and admins, by key.
    let mut members = vec![id.key.clone()];
    for m in roster.members.values().filter(|m| m.role != Role::Member) {
        if !members.contains(&m.key) {
            members.push(m.key.clone());
        }
    }
    members.truncate(MAX_MEMBERS);
    let mut t = Ticket {
        v: 1,
        ticket_id,
        team_id: roster.team_id.clone(),
        secret,
        mode: mode.into(),
        inviter: id.key.clone(),
        relay_hint,
        addrs,
        members,
        expires_at,
        sig: String::new(),
    };
    t.sig = id.sign(&value(&t));
    t
}

/// The `link` entry body for `t`; `hashed` is `mint`'s second value.
pub fn link_body(t: &Ticket, hashed: Option<&str>) -> Value {
    json!({"ticket_id": t.ticket_id, "secret": hashed.unwrap_or(&t.secret),
           "mode": t.mode, "once": hashed.is_some(), "expires_at": t.expires_at})
}

fn value(t: &Ticket) -> Value {
    serde_json::to_value(t).expect("a ticket serializes")
}

pub fn encode(t: &Ticket) -> String {
    BASE64URL_NOPAD.encode(&canonical(&value(t)))
}

/// A ticket from the bare ticket, its web link, the full join line, or its app link.
pub fn decode(s: &str) -> Result<Ticket> {
    let s = s.trim();
    let bare = match s.split_once("/j/#") {
        Some((_, t)) => t,
        None => s.strip_prefix(APP).unwrap_or(s),
    };
    let t: Ticket = BASE64URL_NOPAD
        .decode(bare.trim().as_bytes())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or("bad_ticket")?;
    if t.v != 1 {
        return Err("bad_ticket".into());
    }
    Ok(t)
}

/// `Ok` when `t` is signed by its inviter, names this team, and names a live link of the
/// roster that its inviter signed, in its mode and with its secret (hashed for a once link).
pub fn verify(t: &Ticket, roster: &Roster) -> Result<()> {
    let link = roster.links.get(&t.ticket_id);
    let ok = identity::verify(&t.inviter, &value(t), &t.sig)
        && t.team_id == roster.team_id
        && link.is_some_and(|l| {
            let secret = if l.once {
                hash(&t.secret)
            } else {
                t.secret.clone()
            };
            l.signer == t.inviter && l.mode == t.mode && l.secret == secret
        });
    if ok {
        Ok(())
    } else {
        Err("bad_ticket".into())
    }
}

/// The join line a person pastes.
pub fn line(t: &Ticket) -> String {
    join_line(&encode(t))
}

/// The join line for an `encode`d ticket.
pub fn join_line(encoded: &str) -> String {
    format!("{LINE}{WEB}{encoded}")
}

/// The link the Lightbulb app opens, for an `encode`d ticket.
pub fn app_link(encoded: &str) -> String {
    format!("{APP}{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roster::{replay, Entry, Kind};

    fn id() -> Identity {
        let dir = tempfile::tempdir().unwrap();
        Identity::load_or_create(dir.path()).unwrap()
    }

    /// Founder F with member X; X's minted ticket in `mode`, its link in the roster.
    fn team(mode: &str, once: bool) -> (Identity, Identity, Roster, Ticket) {
        let (f, x) = (id(), id());
        let g = Entry::signed(
            &f,
            vec![],
            Kind::Admit,
            json!({"member": f.key, "name": "f", "person_id": f.key, "role": "founder",
                   "kind": "computer", "team_name": "acme"}),
            1,
        );
        let ax = Entry::signed(
            &f,
            vec![g.hash()],
            Kind::Admit,
            json!({"member": x.key, "name": "x", "person_id": x.key, "kind": "computer"}),
            2,
        );
        let before = replay(&[g.clone(), ax.clone()]);
        let (t, hashed) = mint(&x, &before, mode, once, 100, (String::new(), vec![]));
        let l = Entry::signed(
            &x,
            vec![ax.hash()],
            Kind::Link,
            link_body(&t, hashed.as_deref()),
            3,
        );
        let r = replay(&[g, ax, l]);
        (f, x, r, t)
    }

    #[test]
    fn ticket_round_trips_through_every_link_form() {
        let (_, x, r, t) = team("ask", false);
        let bare = encode(&t);
        let web = format!("{WEB}{bare}");
        for s in [bare.clone(), line(&t), web, app_link(&bare)] {
            assert_eq!(decode(&s).unwrap(), t);
        }
        verify(&t, &r).unwrap();
        assert_eq!(t.members[0], x.key);
        assert_eq!(t.members.len(), 2);
        assert!(t.ticket_id.starts_with(&format!("{}.", x.short)));
        assert_eq!(t.expires_at, 100 + 7 * 86_400);
        assert!(decode("Join Lightbulb Connect: nonsense").is_err());
        // A once ticket carries the secret; its link only the hash.
        let (_, _, r, t) = team("ask", true);
        assert_eq!(r.links[&t.ticket_id].secret, hash(&t.secret));
        assert_eq!(t.expires_at, 100 + 86_400);
        verify(&t, &r).unwrap();
        let mut wrong = t;
        wrong.secret = "guess".into();
        assert!(verify(&wrong, &r).is_err());
    }

    #[test]
    fn ticket_mode_is_covered_by_the_signature() {
        let (_, _, r, t) = team("ask", false);
        let mut open = decode(&encode(&t)).unwrap();
        open.mode = "open".into();
        assert!(!identity::verify(&open.inviter, &value(&open), &open.sig));
        assert!(verify(&open, &r).is_err());
    }

    #[test]
    fn ticket_must_match_its_links_issuer_and_mode() {
        // F knows X's link secret (the roster carries it) and validly signs tickets with it.
        let (f, x, r, t) = team("ask", false);
        let forge = |by: &Identity, mode| {
            sign(
                by,
                &r,
                t.ticket_id.clone(),
                t.secret.clone(),
                mode,
                t.expires_at,
                (String::new(), vec![]),
            )
        };
        assert!(verify(&forge(&f, "open"), &r).is_err());
        assert!(verify(&forge(&f, "ask"), &r).is_err());
        // X itself cannot turn its `ask` link into an `open` one.
        assert!(verify(&forge(&x, "open"), &r).is_err());
        verify(&forge(&x, "ask"), &r).unwrap();
        // Another team's roster.
        let (_, _, other, _) = team("ask", false);
        assert!(verify(&t, &other).is_err());
    }
}
