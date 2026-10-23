//! Sign-in proofs: a compact EdDSA JWS from the Lightbulb issuer naming a person (or, for the
//! workspace member, a workspace) and the computer key it was issued to. Verified offline
//! against the compiled-in issuer keys; they live only in the roster (`merge`, `proof`, an
//! `admit`'s `proof`). Also the issuer's HTTP calls (`post`) and the daemon's renewal (`tick`).

use crate::daemon::{ipc, net_ops, Daemon};
use crate::identity::Identity;
use crate::roster::Kind;
use crate::Result;
use data_encoding::BASE64URL_NOPAD;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A token past this is refused unread (its `admit` must fit a roster entry).
pub const MAX_TOKEN: usize = 4096;
/// A proof lives at most this long.
const MAX_LIFE_S: i64 = 86_400;
/// A proof this old is renewed.
pub const RENEW_AT_S: i64 = 20 * 3600;
/// A failed renewal is tried again after this.
const RETRY_S: i64 = 3600;
/// How far ahead of our clock an issuer may stamp `iat` (clock skew).
const IAT_SKEW_S: i64 = 300;
/// Every issuer call: connect, then read, each at most this.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// An issuer answer past this is refused.
const MAX_BODY: u64 = 64 * 1024;

/// The issuer's public keys by `kid`: prod `p1`, prod `p2` and `dev` (the dev backend's).
// replaced at the key ceremony (Plan C): throwaway keys whose private halves were never kept,
// so a release build verifies no proof yet.
pub const ISSUER_KEYS: [(&str, [u8; 32]); 3] = [
    (
        "p1",
        [
            58, 183, 239, 193, 95, 105, 135, 238, 204, 249, 174, 2, 14, 100, 110, 55, 164, 217,
            103, 219, 116, 73, 173, 182, 77, 114, 136, 220, 212, 148, 32, 139,
        ],
    ),
    (
        "p2",
        [
            254, 120, 176, 104, 134, 90, 45, 218, 55, 132, 208, 61, 28, 217, 171, 204, 238, 175,
            47, 48, 61, 246, 109, 60, 0, 48, 172, 44, 130, 54, 131, 97,
        ],
    ),
    (
        "dev",
        [
            57, 137, 116, 226, 106, 132, 1, 214, 36, 179, 254, 33, 155, 201, 191, 68, 231, 209,
            180, 3, 180, 170, 35, 164, 113, 64, 25, 246, 104, 1, 120, 48,
        ],
    ),
];

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
    ISSUER_KEYS.iter().find(|(k, _)| *k == kid).map(|(_, b)| *b)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProofKind {
    /// A person's computer: `sub` the person, `workspace` the one they are signed into.
    Computer,
    /// The workspace member: `sub` is `ws:<workspace.id>`, `agents` its agents' names.
    Workspace,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProofWorkspace {
    pub id: String,
    pub name: String,
}

/// A verified proof's claims (and the token they came from, never serialized or logged).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Proof {
    pub iss: String,
    /// From the header.
    #[serde(default)]
    pub kid: String,
    pub iat: i64,
    pub exp: i64,
    pub sub: String,
    pub name: String,
    pub computer: String,
    pub kind: ProofKind,
    #[serde(default)]
    pub workspace: Option<ProofWorkspace>,
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(skip)]
    pub token: String,
}

/// Whether `s` has 1..=`max` characters.
fn sized(s: &str, max: usize) -> bool {
    !s.is_empty() && s.chars().count() <= max
}

/// `token`'s claims when it is a well-formed proof for `member_key`, whatever the time: at most
/// `MAX_TOKEN` bytes, header `{alg: EdDSA, kid}` naming an issuer key whose signature over
/// `header.payload` holds, a life of at most a day, `computer` = `member_key`, a `workspace`
/// proof with its workspace and `sub` = `ws:<workspace.id>`, a `computer` one with no agents.
/// The roster's check (replay holds no clock; `Roster::lapse` drops expired ones).
pub fn parse(token: &str, member_key: &str) -> Result<Proof> {
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
    let kid = header["kid"].as_str().unwrap_or_default();
    if header["alg"] != "EdDSA" {
        return Err("proof is not EdDSA".into());
    }
    let key = issuer_key(kid).ok_or("proof names an unknown issuer key")?;
    let key = VerifyingKey::from_bytes(&key).map_err(|_| "bad issuer key")?;
    let sig = Signature::from_slice(&b64(s)?).map_err(|_| "proof has a bad signature")?;
    key.verify_strict(format!("{h}.{p}").as_bytes(), &sig)
        .map_err(|_| "proof has a bad signature")?;
    let mut c: Proof = serde_json::from_slice(&b64(p)?)?;
    c.kid = kid.to_string();
    c.token = token.to_string();
    let ws_ok = match c.kind {
        ProofKind::Workspace => c
            .workspace
            .as_ref()
            .is_some_and(|w| !w.id.is_empty() && c.sub == format!("ws:{}", w.id)),
        ProofKind::Computer => c.agents.is_empty(),
    };
    let bounded = sized(&c.sub, 256)
        && sized(&c.name, 64)
        && c.workspace
            .as_ref()
            .is_none_or(|w| sized(&w.id, 128) && sized(&w.name, 64))
        && c.agents.len() <= 6
        && c.agents.iter().all(|a| sized(a, 32));
    if c.iat >= c.exp || c.exp - c.iat > MAX_LIFE_S {
        Err("proof lives more than a day".into())
    } else if c.computer != member_key {
        Err("proof is for another computer".into())
    } else if !ws_ok || !bounded {
        Err("proof claims do not fit".into())
    } else {
        Ok(c)
    }
}

/// `parse`, and in force at `now`: `iat <= now <= exp` (`iat` up to `IAT_SKEW_S` ahead).
pub fn verify(token: &str, member_key: &str, now: i64) -> Result<Proof> {
    let c = parse(token, member_key)?;
    if c.iat > now + IAT_SKEW_S || now > c.exp {
        return Err("proof is not in force".into());
    }
    Ok(c)
}

/// `POST url` with JSON `body`: the status and the JSON answer (`null` when it is none).
/// https only in release builds (debug: the tests' loopback issuer), no redirects, 10 s to
/// connect and to read, at most 64 KiB.
pub fn post(url: &str, body: &Value) -> Result<(u16, Value)> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(HTTP_TIMEOUT))
        .timeout_recv_response(Some(HTTP_TIMEOUT))
        .timeout_recv_body(Some(HTTP_TIMEOUT))
        .https_only(!cfg!(debug_assertions))
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into();
    let mut r = agent
        .post(url)
        .header("content-type", "application/json")
        .send(body.to_string())
        .map_err(|e| e.to_string())?;
    let status = r.status().as_u16();
    let text = r
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .map_err(|e| e.to_string())?;
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

/// `{issuer}/<path>`.
pub fn route(issuer: &str, path: &str) -> String {
    format!("{}/{path}", issuer.trim_end_matches('/'))
}

/// `proof/refresh`'s body: `{proof, at, sig}`, `sig` by this computer's key over `{proof, at}`.
pub fn refresh_body(id: &Identity, token: &str, at: i64) -> Value {
    let mut v = json!({"proof": token, "at": at});
    // `sign` is over `identity::canonical` of the object without `sig`.
    v["sig"] = id.sign(&v).into();
    v
}

/// On the daemon's 60 s timer: drops expired proofs from the cached roster (their badge and
/// workspace), and once this computer's own proof is `RENEW_AT_S` old asks the issuer for a
/// new one (off the async threads), writing it as a `proof` entry; a failure tries again
/// hourly. `retry_at`: no call before it.
pub fn tick(d: &Arc<Daemon>, retry_at: &Arc<AtomicI64>) {
    let now = ipc::now() as i64;
    let (token, issuer) = {
        let mut team = d.team.lock().unwrap();
        team.roster.lapse(now);
        let own = team.roster.members.get(&d.identity.key);
        let Some(p) = own.and_then(|m| m.proof.as_ref()) else {
            return;
        };
        if now < p.iat + RENEW_AT_S || now < retry_at.load(Ordering::SeqCst) {
            return;
        }
        (p.token.clone(), team.config.issuer.clone())
    };
    // Pessimistic: no second call while this one runs.
    retry_at.store(now + RETRY_S, Ordering::SeqCst);
    let (d, retry_at) = (d.clone(), retry_at.clone());
    tokio::task::spawn_blocking(move || match renew(&d, &token, &issuer, now) {
        Ok(kid) => {
            retry_at.store(0, Ordering::SeqCst);
            eprintln!("proof: renewed (kid {kid})");
        }
        Err(e) => eprintln!("proof: renewal failed, retrying in an hour: {e}"),
    });
}

/// One renewal: `proof/refresh`, the answer verified, then written. Its `kid`.
fn renew(d: &Daemon, token: &str, issuer: &str, now: i64) -> Result<String> {
    let body = refresh_body(&d.identity, token, now);
    let (status, v) = post(&route(issuer, "proof/refresh"), &body)?;
    let new = v["proof"].as_str().filter(|_| status == 200);
    let new = new.ok_or_else(|| format!("issuer answered {status}"))?;
    let kid = verify(new, &d.identity.key, ipc::now() as i64)?.kid;
    record(d, new)?;
    Ok(kid)
}

/// `identity.proof`'s write: `merge` while this computer's `person_id` is its own key, else a
/// `proof` renewal. Refuses a proof of another kind, or for another person than the merged one.
pub fn record(d: &Daemon, token: &str) -> Result<Proof> {
    let me = &d.identity.key;
    let now = ipc::now() as i64;
    let c = verify(token, me, now)?;
    if c.kind != ProofKind::Computer {
        return Err("not a computer proof".into());
    }
    let mut team = d.team.lock().unwrap();
    let store = d.store.lock().unwrap();
    let pid = team.roster.person_of(me).to_string();
    let (kind, body) = if pid == *me {
        let body = json!({"member": me, "person_id": c.sub, "proof": token});
        (Kind::Merge, body)
    } else if pid == c.sub {
        (Kind::Proof, json!({"member": me, "proof": token}))
    } else {
        return Err("proof is for another person".into());
    };
    net_ops::write(d, &mut team, &store, kind, body, now)?;
    eprintln!("proof: recorded (kid {}, sub {})", c.kid, c.sub);
    Ok(c)
}

/// The tests' issuer (`tests/common/issuer.rs`), for the unit tests.
#[cfg(test)]
#[path = "../tests/common/issuer.rs"]
pub(crate) mod test_issuer;

#[cfg(test)]
mod tests {
    use super::test_issuer as issuer;
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn claims(computer: &str) -> Value {
        json!({"iss": "test", "iat": NOW, "exp": NOW + 86_400, "sub": "user_1", "name": "Prasiddha",
               "computer": computer, "kind": "computer",
               "workspace": {"id": "W1", "name": "The Lightbulb Company"}, "agents": []})
    }

    #[test]
    fn proof_verifies_offline_and_rejects_wrong_computer_or_expired() {
        let t = issuer::mint(&claims("k1"));
        let p = verify(&t, "k1", NOW + 10).unwrap();
        assert_eq!((p.sub.as_str(), p.kid.as_str()), ("user_1", TEST_ISSUER.0));
        assert_eq!(p.workspace.unwrap().id, "W1");
        assert!(verify(&t, "k2", NOW).is_err(), "another computer");
        assert!(verify(&t, "k1", NOW + 86_401).is_err(), "expired");
        assert!(verify(&t, "k1", NOW - 3600).is_err(), "not yet issued");
        assert!(parse(&t, "k1").is_ok(), "parse holds no clock");
        // A day and a second of life; another key; a tampered payload; too large.
        let mut c = claims("k1");
        c["exp"] = json!(NOW + 86_401);
        assert!(verify(&issuer::mint(&c), "k1", NOW).is_err());
        let mut parts: Vec<&str> = t.split('.').collect();
        let other = BASE64URL_NOPAD.encode(br#"{"alg":"EdDSA","kid":"p1"}"#);
        parts[0] = &other;
        assert!(verify(&parts.join("."), "k1", NOW).is_err());
        let forged = t.replacen('.', ".e30", 1);
        assert!(verify(&forged, "k1", NOW).is_err());
        assert!(verify(&"a".repeat(MAX_TOKEN + 1), "k1", NOW).is_err());
        // A computer proof names no agents.
        let mut c = claims("k1");
        c["agents"] = json!(["Friday"]);
        assert!(verify(&issuer::mint(&c), "k1", NOW).is_err());
    }

    #[test]
    fn workspace_proof_sub_must_be_ws_of_its_workspace_id() {
        let mut c = claims("k1");
        c["kind"] = json!("workspace");
        c["agents"] = json!(["Friday", "Engineering"]);
        c["sub"] = json!("ws:W2");
        assert!(verify(&issuer::mint(&c), "k1", NOW).is_err());
        c["sub"] = json!("ws:W1");
        let p = verify(&issuer::mint(&c), "k1", NOW).unwrap();
        assert_eq!((p.kind, p.agents.len()), (ProofKind::Workspace, 2));
        c.as_object_mut().unwrap().remove("workspace");
        assert!(verify(&issuer::mint(&c), "k1", NOW).is_err());
    }
}
