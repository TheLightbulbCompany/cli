//! The invite code: four words that stand for a whole ticket. The inviter's daemon binds a
//! second iroh endpoint whose secret key is derived from the code and, over a dedicated ALPN,
//! hands the ticket to the first caller; the joiner derives the same public key and dials it.
//! Single use: the listener ends after one hand-off, after `MAX_BAD` bad callers, after
//! `LIFE_S` (10 minutes) or when the ticket expires. Nothing listens when no code invite is live, and the code is never logged.

use crate::daemon::Daemon;
use crate::net;
use argon2::{Algorithm, Argon2, Params, Version};
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, RelayMode, SecretKey};
use serde_json::Value;
use std::time::Duration;

pub const ALPN: &[u8] = b"lightbulb/code/1";
/// What the joiner sends; anything else from a caller counts as a bad request.
const ASK: &[u8] = b"ticket";
const INFO: &[u8] = b"lightbulb invite code v1";
/// Bad or failed callers a listener answers before it closes.
const MAX_BAD: u32 = 5;
/// Code listeners live at once (an older one closes for a newer).
const MAX_LIVE: usize = 8;
const STEP: Duration = Duration::from_secs(10);
/// How long a joiner tries to reach the inviter.
const FETCH: Duration = Duration::from_secs(15);
/// A code lives this long, or until its ticket expires if that comes first.
pub const LIFE_S: i64 = 600;
/// Argon2id memory (KiB), passes and lanes: a guess costs 64 MiB and about a second, so a relay
/// operator who sees the endpoint key cannot afford to search the ~41-bit space for the secret.
/// Debug builds (the tests) use 1 MiB.
pub const MEM_KIB: u32 = if cfg!(debug_assertions) { 1024 } else { 65536 };
const PASSES: u32 = 3;
/// The ticket's size cap on the wire.
const MAX_TICKET: usize = 8192;

/// The EFF short wordlist 1 (https://www.eff.org/dice, CC BY 3.0 US, 1,296 words) less its
/// hyphenated words, which would clash with the separator: 1,295 words, 41.3 bits per code.
const WORDS: &str = include_str!("words.txt");

fn words() -> impl Iterator<Item = &'static str> {
    WORDS.lines()
}

/// How a person handed over the invite; `cli::join` knows which form it was given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    Code,
    Link,
    Line,
}

/// A new code: four random words joined by `-`.
pub fn generate() -> String {
    let n = words().count() as u64;
    let pick: Vec<&str> = (0..4)
        .map(|_| {
            words()
                .nth((rand::random::<u64>() % n) as usize)
                .unwrap_or_default()
        })
        .collect();
    pick.join("-")
}

/// `s` as a code: lowercase, four listed words split by any run of spaces, hyphens, dots or
/// other non-letters; `None` for anything else.
pub fn normalize(s: &str) -> Option<String> {
    if s.len() > 100 {
        return None;
    }
    let s = s.to_lowercase();
    let parts: Vec<&str> = s
        .split(|c: char| !c.is_ascii_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    (parts.len() == 4 && parts.iter().all(|p| words().any(|w| w == *p))).then(|| parts.join("-"))
}

/// A release build always derives with 64 MiB; this fails the build, not a test, if it doesn't.
const _: () = assert!(cfg!(debug_assertions) || MEM_KIB == 65536);

/// The `LIFE_S` window `t` (unix seconds) falls in. A code lives at most one window, so the
/// joiner finds it under its own window or the one before.
pub fn window(t: i64) -> i64 {
    t.div_euclid(LIFE_S)
}

/// The endpoint key a normalized `code` stands for in `window`: Argon2id over the code, salted
/// with a context string and the window, so a table of every code's key goes stale in ten
/// minutes. Slow on purpose (see `MEM_KIB`): call it off the async threads.
pub fn secret(code: &str, window: i64) -> SecretKey {
    let params = Params::new(MEM_KIB, PASSES, 1, Some(32)).expect("fixed Argon2 parameters");
    let mut okm = [0u8; 32];
    let salt = [INFO, &window.to_be_bytes()].concat();
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(code.as_bytes(), &salt, &mut okm)
        .expect("Argon2 with a 32-byte output and a long enough salt");
    SecretKey::from_bytes(&okm)
}

/// `secret` on a blocking thread.
async fn secret_async(code: &str, window: i64) -> Option<SecretKey> {
    let code = code.to_string();
    tokio::task::spawn_blocking(move || secret(&code, window))
        .await
        .ok()
}

/// Adds `code` to a successful `team.invite` answer `body` and serves its ticket: nothing
/// when the invite has none (a person link), on a workspace member, or when no endpoint
/// binds (the printed line still works).
pub async fn offer(d: &Daemon, body: &mut Value) {
    let (Some(ticket), Some(expires_at)) = (body["ticket"].as_str(), body["expires_at"].as_i64())
    else {
        return;
    };
    if d.cloud.is_some() {
        return;
    }
    let now = crate::daemon::ipc::now() as i64;
    let code = generate();
    let Some(key) = secret_async(&code, window(now)).await else {
        return;
    };
    let urls = d.peers.relays.lock().unwrap().urls.clone();
    let mut b = Endpoint::builder(presets::Minimal)
        .secret_key(key)
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(RelayMode::Custom(
            urls.into_iter().map(net::relay_config).collect(),
        ));
    if d.peers.knobs().relay_only {
        b = b.clear_ip_transports();
    }
    let Ok(ep) = b.bind().await else {
        eprintln!("invite: the code could not be served");
        return;
    };
    let task = tokio::spawn(serve(ep, ticket.to_string(), expires_at.min(now + LIFE_S)));
    let mut live = d.codes.lock().unwrap();
    live.retain(|t| !t.is_finished());
    if live.len() >= MAX_LIVE {
        live.remove(0).abort();
    }
    live.push(task);
    body["code"] = code.into();
    body["code_secs"] = (expires_at.min(now + LIFE_S) - now).into();
}

/// Hands `ticket` to the first caller that asks for it, then closes; also closes after
/// `MAX_BAD` bad callers or at `expires_at`.
async fn serve(ep: Endpoint, ticket: String, expires_at: i64) {
    let left = expires_at
        .saturating_sub(crate::daemon::ipc::now() as i64)
        .max(0) as u64;
    let end = tokio::time::Instant::now() + Duration::from_secs(left);
    let mut bad = 0;
    while bad < MAX_BAD {
        let incoming = tokio::select! {
            i = ep.accept() => i,
            _ = tokio::time::sleep_until(end) => None,
        };
        let Some(incoming) = incoming else { break };
        if hand_off(incoming, &ticket).await {
            break;
        }
        bad += 1;
    }
    ep.close().await;
}

/// Whether one caller asked correctly and got the ticket.
async fn hand_off(incoming: iroh::endpoint::Incoming, ticket: &str) -> bool {
    let run = async {
        let conn = incoming.accept().ok()?.await.ok()?;
        let (mut send, mut recv) = conn.accept_bi().await.ok()?;
        if recv.read_to_end(ASK.len() + 1).await.ok()? != ASK {
            return None;
        }
        send.write_all(ticket.as_bytes()).await.ok()?;
        send.finish().ok()?;
        // Until the caller has it all, so the close that follows cuts nothing off.
        send.stopped().await.ok()?;
        Some(())
    };
    tokio::time::timeout(STEP, run)
        .await
        .ok()
        .flatten()
        .is_some()
}

/// The ticket the inviter of `code` (normalized) serves, or `None` when no one does: a wrong
/// or used code, or an inviter that is off.
pub async fn fetch(d: &Daemon, ep: &Endpoint, code: &str) -> Option<String> {
    let urls = d.peers.relays.lock().unwrap().urls.clone();
    let w = window(crate::daemon::ipc::now() as i64);
    let (now_key, last_key) = tokio::join!(secret_async(code, w), secret_async(code, w - 1));
    // Both windows at once: the one nobody serves only ends at the timeout.
    let mut tries = tokio::task::JoinSet::new();
    for key in [now_key?, last_key?] {
        let addr = urls
            .iter()
            .cloned()
            .fold(EndpointAddr::new(key.public()), |a, u| a.with_relay_url(u));
        let ep = ep.clone();
        tries.spawn(tokio::time::timeout(FETCH, async move {
            let conn = ep.connect(addr, ALPN).await.ok()?;
            let (mut send, mut recv) = conn.open_bi().await.ok()?;
            send.write_all(ASK).await.ok()?;
            send.finish().ok()?;
            let got = recv.read_to_end(MAX_TICKET).await.ok()?;
            String::from_utf8(got).ok()
        }));
    }
    while let Some(done) = tries.join_next().await {
        if let Ok(Ok(Some(ticket))) = done {
            return Some(ticket);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_are_plain_and_enough() {
        assert!(words().count() >= 1024, "four words must carry 40 bits");
        assert!(words().all(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_lowercase())));
    }

    #[test]
    fn a_code_round_trips_through_every_separator() {
        let c = generate();
        assert_eq!(normalize(&c), Some(c.clone()));
        assert_eq!(
            normalize(&c.replace('-', " ").to_uppercase()),
            Some(c.clone())
        );
        assert_eq!(normalize(&c.replace('-', ".")), Some(c.clone()));
        assert_eq!(
            normalize(&format!("  {}  ", c.replace('-', " - "))),
            Some(c.clone())
        );
        assert_eq!(secret(&c, 7).public(), secret(&c, 7).public());
        let other = (0..).map(|_| generate()).find(|o| *o != c).unwrap();
        assert_ne!(secret(&c, 7).public(), secret(&other, 7).public());
    }

    #[test]
    fn the_derivation_is_deterministic_and_per_code() {
        let (a, b) = ("alpha-bravo-charlie-delta", "alpha-bravo-charlie-echo");
        assert_eq!(secret(a, 7).public(), secret(a, 7).public());
        assert_ne!(secret(a, 7).public(), secret(b, 7).public());
        // A precomputed table goes stale: the same code has another key in the next window.
        assert_ne!(secret(a, 7).public(), secret(a, 8).public());
        assert_eq!(window(599), 0);
        assert_eq!(window(600), 1);
    }

    #[test]
    fn what_is_not_a_code_is_refused() {
        for s in [
            "",
            "one two three",
            "abandon-abandon-abandon-abandon",
            "a-b-c-d",
        ] {
            assert_eq!(normalize(s), None, "{s}");
        }
        let w: Vec<&str> = words().take(5).collect();
        assert_eq!(normalize(&w.join("-")), None, "five words");
    }
}
