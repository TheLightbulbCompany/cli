//! Sign-in proofs: a compact EdDSA JWS from Lightbulb (`lightbulb login`) naming the user a
//! computer belongs to, `{iat, exp, sub: <user id>, computer: <member key>}`. A computer keeps
//! its own (`login::stored_proof`) and shows it in its `hello`; a workspace member verifies a
//! peer's offline against the issuer's public key and answers only the people in its
//! `members`. Nothing here calls the issuer.

use crate::daemon::Daemon;
use crate::Result;
use data_encoding::BASE64URL_NOPAD;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;

/// A token past this is refused unread.
pub const MAX_TOKEN: usize = 4096;
/// A proof lives at most this long (Lightbulb issues 30 days).
const MAX_LIFE_S: i64 = 31 * 86_400;
/// How far ahead of our clock an issuer may stamp `iat` (clock skew).
const IAT_SKEW_S: i64 = 300;
/// `<kid>:<base64url of the 32-byte key>`: another issuer key (a dev backend's, or the one a
/// workspace runtime is handed). `GET /api/v1/network/issuer` answers it as `env`.
pub const ISSUER_ENV: &str = "LIGHTBULB_ISSUER_KEY";

/// The issuer's public keys by `kid`, baked in at the key ceremony (owner: fill from the prod
/// backend's `GET /api/v1/network/issuer`). Until then only `LIGHTBULB_ISSUER_KEY` verifies.
pub const ISSUER_KEYS: &[(&str, [u8; 32])] = &[];

/// Debug builds only: the tests' issuer (its private half is in `tests/common/issuer.rs`).
#[cfg(debug_assertions)]
pub const TEST_ISSUER: (&str, [u8; 32]) = (
    "lightbulb-debug-test-issuer",
    [
        138, 219, 190, 38, 233, 189, 22, 141, 193, 77, 204, 211, 246, 200, 175, 230, 171, 56, 245,
        99, 111, 79, 53, 134, 127, 74, 234, 110, 185, 156, 217, 91,
    ],
);

fn issuer_key(kid: &str) -> Option<[u8; 32]> {
    #[cfg(debug_assertions)]
    if kid == TEST_ISSUER.0 {
        return Some(TEST_ISSUER.1);
    }
    if let Ok(v) = std::env::var(ISSUER_ENV) {
        if let Some((k, key)) = v.split_once(':') {
            let raw = BASE64URL_NOPAD.decode(key.trim().as_bytes()).ok()?;
            if k == kid {
                return raw.try_into().ok();
            }
        }
    }
    ISSUER_KEYS.iter().find(|(k, _)| *k == kid).map(|(_, b)| *b)
}

/// A verified proof's claims.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Proof {
    pub iat: i64,
    pub exp: i64,
    pub sub: String,
    pub computer: String,
    /// When the person confirmed in the browser; renewals carry it unchanged.
    #[serde(default)]
    pub auth_time: Option<i64>,
}

/// `token`'s claims when it is a proof from a known issuer key for `member_key`, in force at
/// `now` (`iat` at most `IAT_SKEW_S` ahead), living at most `MAX_LIFE_S`.
pub fn verify(token: &str, member_key: &str, now: i64) -> Result<Proof> {
    if token.len() > MAX_TOKEN {
        return Err("proof is too large".into());
    }
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(s), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("proof is not a compact JWS".into());
    };
    let b64 = |x: &str| {
        BASE64URL_NOPAD
            .decode(x.as_bytes())
            .map_err(|_| "proof is not base64url")
    };
    let header: Value = serde_json::from_slice(&b64(h)?)?;
    if header["alg"] != "EdDSA" {
        return Err("proof is not EdDSA".into());
    }
    let kid = header["kid"].as_str().unwrap_or_default();
    let key = issuer_key(kid).ok_or("proof names an unknown issuer key")?;
    let key = VerifyingKey::from_bytes(&key).map_err(|_| "bad issuer key")?;
    let sig = Signature::from_slice(&b64(s)?).map_err(|_| "proof has a bad signature")?;
    key.verify_strict(format!("{h}.{p}").as_bytes(), &sig)
        .map_err(|_| "proof has a bad signature")?;
    let c: Proof = serde_json::from_slice(&b64(p)?)?;
    if c.computer != member_key {
        Err("proof is for another computer".into())
    } else if c.iat >= c.exp || c.exp - c.iat > MAX_LIFE_S || !c.sub.starts_with("user_") {
        Err("proof claims do not fit".into())
    } else if c.iat > now + IAT_SKEW_S || now > c.exp {
        Err("proof is not in force".into())
    } else {
        Ok(c)
    }
}

/// `token`'s claims, unchecked: this computer reading its OWN proof's `exp` (it may hold no
/// issuer key to verify it with), or the directory showing who a proof says it is. Never a gate.
pub fn peek(token: &str) -> Option<Proof> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&BASE64URL_NOPAD.decode(payload.as_bytes()).ok()?).ok()
}

/// The online members whose shown proof (their latest `hello`'s) names one of `people`: the
/// computers a workspace member answers (`members`).
pub fn admitted(d: &Daemon, people: &HashSet<String>) -> HashSet<String> {
    let now = crate::daemon::ipc::now() as i64;
    let revoked = d.cloud.as_ref().map(|c| c.declared().revoked);
    d.proofs
        .lock()
        .unwrap()
        .iter()
        .filter(|(key, _)| !revoked.as_ref().is_some_and(|r| r.contains(*key)))
        .filter(|(key, token)| verify(token, key, now).is_ok_and(|p| people.contains(&p.sub)))
        .map(|(key, _)| key.clone())
        .collect()
}

/// The tests' issuer (`tests/common/issuer.rs`), for the unit tests.
#[cfg(test)]
#[path = "../tests/common/issuer.rs"]
pub(crate) mod test_issuer;

#[cfg(test)]
mod tests {
    use super::test_issuer as issuer;
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn a_proof_verifies_offline_for_its_computer_and_only_while_in_force() {
        let t = issuer::proof("k1", "user_1", NOW);
        assert_eq!(verify(&t, "k1", NOW + 10).unwrap().sub, "user_1");
        assert!(verify(&t, "k2", NOW).is_err(), "another computer");
        assert!(verify(&t, "k1", NOW + 31 * 86_400).is_err(), "expired");
        assert!(verify(&t, "k1", NOW - 3600).is_err(), "not yet issued");
        let forged = t.replacen('.', ".e30", 1);
        assert!(verify(&forged, "k1", NOW).is_err(), "tampered");
        assert!(verify(&"a".repeat(MAX_TOKEN + 1), "k1", NOW).is_err());
        let long = json!({"iat": NOW, "exp": NOW + 40 * 86_400, "sub": "user_1", "computer": "k1"});
        assert!(
            verify(&issuer::mint(&long), "k1", NOW).is_err(),
            "lives too long"
        );
        let ws = json!({"iat": NOW, "exp": NOW + 86_400, "sub": "ws:W1", "computer": "k1"});
        assert!(verify(&issuer::mint(&ws), "k1", NOW).is_err(), "not a user");
    }
}
