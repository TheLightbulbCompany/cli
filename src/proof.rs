//! Sign-in proofs: a compact EdDSA JWS from Lightbulb (`lightbulb login`) naming the user a
//! computer belongs to, `{iat, exp, sub: <user id>, computer: <member key>}`. A computer keeps
//! its own (`login::stored_proof`) and shows it in its `hello`; a workspace member verifies a
//! peer's offline against the issuer's public key and answers only the people in its
//! `members`. Nothing here calls the issuer.

use crate::Result;
use data_encoding::BASE64URL_NOPAD;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use serde_json::Value;

/// A token past this is refused unread.
pub const MAX_TOKEN: usize = 4096;
/// A proof lives at most this long (Lightbulb issues 30 days).
const MAX_LIFE_S: i64 = 31 * 86_400;
/// How far ahead of our clock an issuer may stamp `iat` (clock skew).
const IAT_SKEW_S: i64 = 300;
/// `<kid>:<base64url of the 32-byte key>`: another issuer key (a dev backend's, or the one a
/// workspace runtime is handed). `GET /api/v1/network/issuer` answers it as `env`.
pub const ISSUER_ENV: &str = "LIGHTBULB_ISSUER_KEY";

/// The issuer's public keys by `kid`: the prod backend's `GET /api/v1/network/issuer` (4 Nov 2026).
/// A dev backend's key still comes from `LIGHTBULB_ISSUER_KEY`.
pub const ISSUER_KEYS: &[(&str, [u8; 32])] = &[(
    "prod1",
    [
        118, 236, 205, 44, 130, 188, 100, 30, 134, 146, 98, 44, 161, 115, 102, 133, 254, 157, 215,
        129, 201, 232, 224, 35, 170, 251, 5, 78, 248, 133, 206, 34,
    ],
)];

/// Debug builds only: the tests' issuer (its private half is in `tests/common/issuer.rs`).
#[cfg(debug_assertions)]
pub const TEST_ISSUER: (&str, [u8; 32]) = (
    "lightbulb-debug-test-issuer",
    [
        138, 219, 190, 38, 233, 189, 22, 141, 193, 77, 204, 211, 246, 200, 175, 230, 171, 56, 245,
        99, 111, 79, 53, 134, 127, 74, 234, 110, 185, 156, 217, 91,
    ],
);

pub(crate) fn issuer_key(kid: &str) -> Option<[u8; 32]> {
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
    /// A workspace proof's workspace (`org_<id>`, equal to `sub`); a person's has none.
    #[serde(default)]
    pub org: Option<String>,
}

/// Whether `p` names a workspace's computer, not a person.
pub fn is_workspace(p: &Proof) -> bool {
    p.sub.starts_with("org_")
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
    } else if c.iat >= c.exp
        || c.exp - c.iat > MAX_LIFE_S
        || !(c.sub.starts_with("user_")
            || is_workspace(&c) && c.org.as_deref() == Some(c.sub.as_str()))
    {
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

/// The tests' issuer (`tests/common/issuer.rs`), for the unit tests.
#[cfg(test)]
#[path = "../tests/common/issuer.rs"]
pub(crate) mod test_issuer;

#[cfg(test)]
mod tests {
    #[test]
    fn the_built_in_prod_key_is_the_one_the_prod_backend_publishes() {
        let k = super::issuer_key("prod1").expect("prod1 is built in");
        assert_eq!(
            data_encoding::BASE64URL_NOPAD.encode(&k),
            "duzNLIK8ZB6GkmIsoXNmhf6d14HJ6OAjqvsFTviFziI"
        );
    }

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

    #[test]
    fn workspace_proof_verifies() {
        let t = issuer::workspace_proof("k1", "org_1", NOW);
        let p = verify(&t, "k1", NOW + 10).unwrap();
        assert_eq!((p.sub.as_str(), p.org.as_deref()), ("org_1", Some("org_1")));
        assert!(is_workspace(&p));
    }

    #[test]
    fn workspace_proof_with_a_different_org_is_refused() {
        let mk = |org: Value| {
            issuer::mint(
                &json!({"iat": NOW, "exp": NOW + 86_400, "sub": "org_1", "org": org, "computer": "k1"}),
            )
        };
        assert!(verify(&mk(json!("org_2")), "k1", NOW).is_err());
        assert!(verify(&mk(Value::Null), "k1", NOW).is_err(), "no org");
    }

    #[test]
    fn a_user_proof_still_verifies_and_has_no_org() {
        let p = verify(&issuer::proof("k1", "user_1", NOW), "k1", NOW).unwrap();
        assert_eq!(p.org, None);
        assert!(!is_workspace(&p));
    }
}
