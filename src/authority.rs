//! The A2A authority object: a compact EdDSA JWS by Lightbulb's issuer (the key that signs
//! sign-in proofs, `proof::issuer_key`) naming who asks for what, carried in an envelope's
//! `a2a.metadata[URI]`. `verify` checks it offline; `bind` ties it to the envelope it rides.

use crate::mail::Meta;
use crate::Result;
use data_encoding::{BASE64URL_NOPAD, HEXLOWER};
use ed25519_dalek::{Signature, VerifyingKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// The A2A extension's URI: the key of the object in `a2a.metadata`.
pub const URI: &str = "https://thelightbulb.company/a2a/ext/authority/v1";
pub const TYP: &str = "lightbulb-a2a-authority";
/// An object lives at most this long.
pub const MAX_LIFE_S: i64 = 86_400;
/// A token past this is refused unread.
pub const MAX_TOKEN: usize = 16 * 1024;
/// How far ahead of our clock the issuer may stamp `iat` (the proof's skew).
const IAT_SKEW_S: i64 = 300;
/// The longest `intent` phase 1 mints, in bytes.
const MAX_INTENT: usize = 2048;

/// A verified authority object. `raw` is the token itself: never printed (`Debug` leaves it out).
#[derive(Clone, PartialEq)]
pub struct Authority {
    pub id: String,
    pub requester_person: String,
    pub requester_org: String,
    pub verified_by: String,
    pub chain: Vec<Hop>,
    pub skill: String,
    pub resource: String,
    pub operands: Value,
    pub intent: String,
    pub iat: i64,
    pub exp: i64,
    pub raw: String,
}

impl std::fmt::Debug for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authority")
            .field("id", &self.id)
            .field("requester_person", &self.requester_person)
            .field("resource", &self.resource)
            .finish_non_exhaustive()
    }
}

/// One hop of the object's `chain`: the first is the requester's.
#[derive(Clone, Debug, PartialEq)]
pub struct Hop {
    pub agent: Option<String>,
    pub session: Option<String>,
    pub person: String,
    pub at: i64,
}

/// What an inbox shows of a verified object (`frame::framed_kind`'s `[A2A]` line).
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub requester: String,
    pub for_whom: String,
    pub intent: String,
    /// Whether the object verified when the summary was made.
    pub verified: bool,
}

impl Authority {
    /// The chain's agents (a hop without one, its person) joined with " → ", for the first hop's person.
    pub fn summary(&self) -> Summary {
        let hop = |h: &Hop| h.agent.clone().unwrap_or_else(|| h.person.clone());
        Summary {
            requester: self.chain.iter().map(hop).collect::<Vec<_>>().join(" → "),
            for_whom: self
                .chain
                .first()
                .map(|h| h.person.clone())
                .unwrap_or_default(),
            intent: self.intent.clone(),
            verified: true,
        }
    }
}

/// The summary of the object `m` carries, verified again at `now` (never read from unverified
/// fields); an empty one when it no longer verifies. `None` for a message with no bound object.
pub fn summary_of(m: &crate::store::Message, now: i64) -> Option<Summary> {
    m.authority_id.as_ref()?;
    let jws = m.a2a.as_ref()?["metadata"][URI].as_str()?;
    Some(verify(jws, now).map_or(
        Summary {
            requester: String::new(),
            for_whom: String::new(),
            intent: String::new(),
            verified: false,
        },
        |a| a.summary(),
    ))
}

/// `"sha256:" + hex(sha256(canonical(operands)))`, as phase 1 hashes them.
pub fn operands_hash(operands: &Value) -> String {
    let digest = Sha256::digest(crate::identity::canonical(operands));
    format!("sha256:{}", HEXLOWER.encode(&digest))
}

/// `token`'s claims when it is a compact EdDSA JWS of at most `max` bytes, signed by a known
/// issuer key, whose protected header `typ` is `typ`. Errors name the check: `size`, `jws`,
/// `alg`, `kid`, `signature`, `typ`. The claims' own checks are the caller's.
pub(crate) fn verify_jws(token: &str, max: usize, typ: &str) -> Result<Value> {
    if token.len() > max {
        return Err("size".into());
    }
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(s), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("jws".into());
    };
    let b64 = |x: &str| BASE64URL_NOPAD.decode(x.as_bytes()).map_err(|_| "jws");
    let (header, payload, sig) = (b64(h)?, b64(p)?, b64(s)?);
    let header: Value = serde_json::from_slice(&header).map_err(|_| "jws")?;
    if !header.is_object() {
        return Err("jws".into());
    }
    if header["alg"] != "EdDSA" {
        return Err("alg".into());
    }
    let kid = header["kid"].as_str().unwrap_or_default();
    let key = crate::proof::issuer_key(kid).ok_or("kid")?;
    let key = VerifyingKey::from_bytes(&key).map_err(|_| "kid")?;
    let sig = Signature::from_slice(&sig).map_err(|_| "signature")?;
    key.verify_strict(format!("{h}.{p}").as_bytes(), &sig)
        .map_err(|_| "signature")?;
    // The issuer puts `typ` in the protected header; a `typ` among the claims is not it.
    if header["typ"] != typ {
        return Err("typ".into());
    }
    Ok(serde_json::from_slice(&payload).map_err(|_| "jws")?)
}

/// `jws`'s claims when it is an authority object from a known issuer key, in force at `now`
/// (`iat` at most `IAT_SKEW_S` ahead), living at most `MAX_LIFE_S`, of the phase-1 shape, its
/// first hop the requester and its `operands_hash` its operands'. Errors name the check.
pub fn verify(jws: &str, now: i64) -> Result<Authority> {
    let c = verify_jws(jws, MAX_TOKEN, TYP)?;
    let a = shape(&c, jws).ok_or("shape")?;
    let on_behalf_of = &c["on_behalf_of"];
    if a.intent.len() > MAX_INTENT {
        Err("intent_length".into())
    } else if a.iat > now + IAT_SKEW_S {
        Err("iat".into())
    } else if a.iat >= a.exp || a.exp - a.iat > MAX_LIFE_S || now >= a.exp {
        Err("exp".into())
    } else if a.chain[0].person != a.requester_person
        || !(on_behalf_of.is_null() || *on_behalf_of == *a.requester_person)
    {
        Err("origin".into())
    } else if c["operation"]["operands_hash"] != operands_hash(&a.operands) {
        Err("operands_hash".into())
    } else {
        Ok(a)
    }
}

/// `c` read as the phase-1 shape, or `None` when it does not fit.
fn shape(c: &Value, raw: &str) -> Option<Authority> {
    let s = |v: &Value| v.as_str().map(String::from);
    let full = |v: &Value| s(v).filter(|x| !x.is_empty());
    let opt = |v: &Value| match v {
        Value::Null => Some(None),
        v => v.as_str().map(|x| Some(x.to_string())),
    };
    let (r, op) = (&c["requester"], &c["operation"]);
    let ok = c["v"] == 1
        && c["policy"].is_null()
        && op["operands"].is_object()
        && op["operands_hash"].is_string()
        && (c["on_behalf_of"].is_null() || c["on_behalf_of"].is_string());
    let chain = c["chain"]
        .as_array()
        .filter(|h| !h.is_empty())?
        .iter()
        .map(|h| {
            Some(Hop {
                agent: opt(&h["agent"])?,
                session: opt(&h["session"])?,
                person: s(&h["person"])?,
                at: h["at"].as_i64()?,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    ok.then_some(Authority {
        id: full(&c["id"])?,
        requester_person: full(&r["person"])?,
        requester_org: full(&r["org"])?,
        verified_by: full(&r["verified_by"])?,
        chain,
        skill: s(&op["skill"])?,
        resource: s(&op["resource"])?,
        operands: op["operands"].clone(),
        intent: s(&c["intent"])?,
        iat: c["iat"].as_i64()?,
        exp: c["exp"].as_i64()?,
        raw: raw.into(),
    })
}

/// The operands an envelope of `kind` binds: `{text}`, and for an `assign` its terms as sent
/// (`done_when`, `deadline_at`, `repo`, `branch`), absent ones left out.
pub fn expected_operands(kind: &str, body: &str, meta: &Meta) -> Value {
    let mut o = json!({"text": body});
    if kind == "assign" {
        let terms = [
            ("done_when", meta.done_when.clone().map(Value::from)),
            ("deadline_at", meta.deadline_at.map(Value::from)),
            ("repo", meta.repo.clone().map(Value::from)),
            ("branch", meta.branch.clone().map(Value::from)),
        ];
        for (k, v) in terms {
            if let Some(v) = v {
                o[k] = v;
            }
        }
    }
    o
}

/// `a` bound to the envelope it rides: its resource is `to`'s (`worker:<short>/<harness>` for an
/// assign to a fresh worker, else `session:<to>`), its skill fits `kind` (`assign`; `message` or
/// `ask_session` for a message), its operands are the envelope's
/// (`expected_operands`), and its requester is the sending computer's proof person
/// (`sender_person`), or the sender relays a hosted chain (two hops, it is the second), or the
/// sender is signed in as the requester's workspace (`sender_workspace`) and relays its hosted
/// agent's note (two hops, the second an agent's, as phase 1 mints it).
/// An authority id is not single-use on the network in this phase: binding it to its sender
/// stops a third party reusing it, and the hosted door enforces single use when it mints.
pub fn bind(
    a: &Authority,
    kind: &str,
    to: &str,
    body: &str,
    meta: &Meta,
    sender_person: Option<&str>,
    sender_workspace: Option<&str>,
) -> Result<()> {
    let resource = match to.strip_suffix(":new") {
        Some(worker) if kind == "assign" => format!("worker:{worker}"),
        _ => format!("session:{to}"),
    };
    if a.resource != resource {
        return Err("resource".into());
    }
    // A `result` or `cancel` carrying an object is refused: phase 1 never mints for them.
    let skill_ok = match kind {
        "assign" => a.skill == "assign",
        "message" => matches!(a.skill.as_str(), "message" | "ask_session"),
        _ => false,
    };
    if !skill_ok {
        return Err("skill".into());
    }
    let want = expected_operands(kind, body, meta);
    if want != a.operands {
        return Err("operands".into());
    }
    let relayed = a.chain.len() == 2 && sender_person == Some(a.chain[1].person.as_str());
    let hosted = a.chain.len() == 2
        && a.chain[1].agent.is_some()
        && sender_workspace == Some(a.requester_org.as_str());
    if sender_person != Some(a.requester_person.as_str()) && !relayed && !hosted {
        return Err("requester".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::Meta;
    use crate::proof::test_issuer as issuer;
    use serde_json::{json, Value};

    const BODY: &str = "fix the build";

    /// A full claim set (requester `user_a`, resource `session:abc/codex:1`, operands
    /// `{text: BODY}`), `over` merged on top, minted by the test issuer.
    fn claims(over: Value) -> Value {
        let operands = json!({"text": BODY});
        let mut c = json!({
            "v": 1, "id": "auth_1", "iat": 1_000, "exp": 1_000 + 3_600,
            "requester": {"person": "user_a", "org": "org_1", "verified_by": "lightbulb"},
            "chain": [{"agent": "agent_1", "session": "s1", "person": "user_a", "at": 1_000}],
            "operation": {"skill": "message", "resource": "session:abc/codex:1",
                          "operands": operands, "operands_hash": operands_hash(&operands)},
            "intent": "ship it", "policy": null,
        });
        for (k, v) in over.as_object().unwrap() {
            c[k] = v.clone();
        }
        c
    }

    fn mint(over: Value) -> String {
        issuer::mint_typ(TYP, &claims(over))
    }

    fn err(r: Result<Authority>) -> String {
        r.map(|_| ()).unwrap_err().to_string()
    }

    #[test]
    fn verifies_a_well_formed_object() {
        let jws = mint(json!({}));
        let a = verify(&jws, 1_500).unwrap();
        assert_eq!(a.id, "auth_1");
        assert_eq!(a.requester_person, "user_a");
        assert_eq!(a.resource, "session:abc/codex:1");
        assert_eq!(
            operands_hash(&a.operands),
            operands_hash(&json!({"text": BODY}))
        );
        assert!(operands_hash(&a.operands).starts_with("sha256:"));
        assert_eq!(a.raw, jws);
        assert_eq!(a.chain[0].agent.as_deref(), Some("agent_1"));
    }

    #[test]
    fn rejects_wrong_typ_expired_long_life_and_bad_sig() {
        let other_typ = issuer::mint_typ("lightbulb-a2a-rules", &claims(json!({})));
        assert_eq!(err(verify(&other_typ, 1_500)), "typ");
        // `typ` only among the claims (no header `typ`) is not an authority object.
        let in_claims = issuer::mint(&claims(json!({"typ": TYP})));
        assert_eq!(err(verify(&in_claims, 1_500)), "typ");
        assert_eq!(err(verify(&mint(json!({})), 1_000 + 3_601)), "exp");
        let long = json!({"exp": 1_000 + MAX_LIFE_S + 1});
        assert_eq!(err(verify(&mint(long), 1_500)), "exp");
        let jws = mint(json!({}));
        let signing = jws.rsplit_once('.').unwrap().0;
        let other = issuer::mint(&json!({"x": 1}));
        let bad = format!("{signing}.{}", other.rsplit_once('.').unwrap().1);
        assert_ne!(bad, jws);
        assert_eq!(err(verify(&bad, 1_500)), "signature");
    }

    #[test]
    fn every_refusal_names_its_check() {
        use data_encoding::BASE64URL_NOPAD as B64;
        let b64 = |v: &str| B64.encode(v.as_bytes());
        let good = mint(json!({}));
        let (h, rest) = good.split_once('.').unwrap();
        let sig = rest.split_once('.').unwrap().1;
        let header = |hd: Value| format!("{}.{}", b64(&hd.to_string()), rest);
        let kid = issuer::KID;
        let with = |f: &dyn Fn(&mut Value)| {
            let mut c = claims(json!({}));
            f(&mut c);
            issuer::mint_typ(TYP, &c)
        };
        let rows: Vec<(&str, String, i64)> = vec![
            ("size", "a".repeat(MAX_TOKEN + 1), 1_500),
            ("jws", format!("{h}.{sig}"), 1_500),
            ("jws", format!("{h}.!!!.{sig}"), 1_500),
            ("jws", format!("{}.{rest}", b64("1")), 1_500),
            ("jws", format!("{}.{rest}", b64("\"x\"")), 1_500),
            (
                "alg",
                header(json!({"alg": "RS256", "kid": kid, "typ": TYP})),
                1_500,
            ),
            (
                "kid",
                header(json!({"alg": "EdDSA", "kid": "nope", "typ": TYP})),
                1_500,
            ),
            ("shape", with(&|c| c["v"] = true.into()), 1_500),
            (
                "shape",
                with(&|c| c["requester"]["person"] = 1.into()),
                1_500,
            ),
            (
                "shape",
                with(&|c| c["requester"]["person"] = "".into()),
                1_500,
            ),
            ("shape", with(&|c| c["requester"]["org"] = "".into()), 1_500),
            (
                "shape",
                with(&|c| c["requester"]["verified_by"] = "".into()),
                1_500,
            ),
            ("shape", with(&|c| c["policy"] = json!({})), 1_500),
            ("shape", with(&|c| c["chain"] = json!([])), 1_500),
            (
                "shape",
                with(&|c| c["operation"]["operands"] = "x".into()),
                1_500,
            ),
            (
                "origin",
                with(&|c| c["on_behalf_of"] = "user_x".into()),
                1_500,
            ),
            (
                "intent_length",
                with(&|c| c["intent"] = "i".repeat(2049).into()),
                1_500,
            ),
            ("exp", good.clone(), 1_000 + 3_600),
        ];
        for (want, jws, now) in rows {
            assert_eq!(err(verify(&jws, now)), want, "row {want}");
        }
        assert_eq!(MAX_TOKEN + 1, 16_385);
        let mine = with(&|c| c["on_behalf_of"] = "user_a".into());
        assert!(verify(&mine, 1_500).is_ok(), "on_behalf_of the requester");
        assert!(verify(&with(&|c| c["intent"] = "i".repeat(2048).into()), 1_500).is_ok());
    }

    #[test]
    fn accepts_iat_300s_ahead_and_rejects_301() {
        assert!(verify(&mint(json!({})), 700).is_ok());
        assert_eq!(err(verify(&mint(json!({})), 699)), "iat");
    }

    #[test]
    fn rejects_origin_mismatch_and_operands_hash() {
        let chain = json!([{"agent": null, "session": null, "person": "user_x", "at": 1_000}]);
        assert_eq!(err(verify(&mint(json!({"chain": chain})), 1_500)), "origin");
        let mut c = claims(json!({}));
        c["operation"]["operands_hash"] = operands_hash(&json!({"text": "other"})).into();
        assert_eq!(
            err(verify(&issuer::mint_typ(TYP, &c), 1_500)),
            "operands_hash"
        );
    }

    #[test]
    fn bind_checks_resource_text_and_requester() {
        let a = verify(&mint(json!({})), 1_500).unwrap();
        let (to, m) = ("abc/codex:1", Meta::default());
        assert!(bind(&a, "message", to, BODY, &m, Some("user_a"), None).is_ok());
        let e = |r: Result<()>| r.unwrap_err().to_string();
        assert_eq!(
            e(bind(
                &a,
                "message",
                "abc/codex:2",
                BODY,
                &m,
                Some("user_a"),
                None
            )),
            "resource"
        );
        assert_eq!(
            e(bind(&a, "message", to, "other", &m, Some("user_a"), None)),
            "operands"
        );
        assert_eq!(
            e(bind(&a, "message", to, BODY, &m, Some("user_b"), None)),
            "requester"
        );
        assert_eq!(
            e(bind(&a, "message", to, BODY, &m, None, None)),
            "requester"
        );
        let mut c = claims(json!({}));
        c["operation"]["skill"] = "assign".into();
        let wrong_skill = verify(&issuer::mint_typ(TYP, &c), 1_500).unwrap();
        assert_eq!(
            e(bind(
                &wrong_skill,
                "message",
                to,
                BODY,
                &m,
                Some("user_a"),
                None
            )),
            "skill"
        );
        // The cloud member relays a hosted agent's ask: chain of two, it is the second hop.
        let chain = json!([
            {"agent": "hosted", "session": null, "person": "user_a", "at": 1_000},
            {"agent": null, "session": null, "person": "user_cloud", "at": 1_001},
        ]);
        let relayed = verify(&mint(json!({"chain": chain})), 1_500).unwrap();
        assert!(bind(&relayed, "message", to, BODY, &m, Some("user_cloud"), None).is_ok());
        assert_eq!(
            e(bind(
                &relayed,
                "message",
                to,
                BODY,
                &m,
                Some("user_b"),
                None
            )),
            "requester"
        );
    }

    #[test]
    fn bind_assign_uses_worker_resource_and_meta_operands() {
        let meta = Meta {
            done_when: Some("tests pass".into()),
            deadline_at: Some(9_000),
            branch: Some("main".into()),
            ..Meta::default()
        };
        let operands = expected_operands("assign", BODY, &meta);
        assert_eq!(
            operands,
            json!({"text": BODY, "done_when": "tests pass", "deadline_at": 9_000, "branch": "main"})
        );
        let mut c = claims(json!({}));
        c["operation"] = json!({"skill": "assign", "resource": "worker:abc/claude",
            "operands": operands, "operands_hash": operands_hash(&operands)});
        let a = verify(&issuer::mint_typ(TYP, &c), 1_500).unwrap();
        let to = "abc/claude:new";
        assert!(bind(&a, "assign", to, BODY, &meta, Some("user_a"), None).is_ok());
        let e = |r: Result<()>| r.unwrap_err().to_string();
        assert_eq!(
            e(bind(
                &a,
                "assign",
                "abc/codex:new",
                BODY,
                &meta,
                Some("user_a"),
                None
            )),
            "resource"
        );
        let later = Meta {
            deadline_at: Some(9_001),
            ..meta.clone()
        };
        assert_eq!(
            e(bind(&a, "assign", to, BODY, &later, Some("user_a"), None)),
            "operands"
        );
        let no_branch = Meta {
            branch: None,
            ..meta
        };
        assert_eq!(
            e(bind(
                &a,
                "assign",
                to,
                BODY,
                &no_branch,
                Some("user_a"),
                None
            )),
            "operands"
        );
    }

    /// A hosted note for `org_1` as phase 1 mints it: the requester's hop, then the hosted
    /// agent's (`chain[1].agent` set), its `over` merged on top.
    fn hosted_note(over: Value) -> Authority {
        let chain = json!([
            {"agent": null, "session": null, "person": "user_a", "at": 1_000},
            {"agent": "agent_9", "session": "run_1", "person": "user_a", "at": 1_001},
        ]);
        let mut c = json!({"chain": chain});
        for (k, v) in over.as_object().unwrap() {
            c[k] = v.clone();
        }
        verify(&mint(c), 1_500).unwrap()
    }

    fn bound(a: &Authority, workspace: &str) -> std::result::Result<(), String> {
        let m = Meta::default();
        bind(a, "message", "abc/codex:1", BODY, &m, None, Some(workspace))
            .map_err(|e| e.to_string())
    }

    #[test]
    fn workspace_sender_binds_its_own_workspaces_hosted_note() {
        assert_eq!(bound(&hosted_note(json!({})), "org_1"), Ok(()));
    }

    #[test]
    fn workspace_sender_cannot_bind_another_workspaces_note() {
        assert_eq!(
            bound(&hosted_note(json!({})), "org_2"),
            Err("requester".into())
        );
    }

    #[test]
    fn workspace_sender_cannot_bind_a_one_hop_note() {
        // `claims`' own chain: the requester's hop alone, an agent on it.
        let a = verify(&mint(json!({})), 1_500).unwrap();
        assert_eq!(bound(&a, "org_1"), Err("requester".into()));
        let three = json!([
            {"agent": null, "session": null, "person": "user_a", "at": 1_000},
            {"agent": "agent_9", "session": "run_1", "person": "user_a", "at": 1_001},
            {"agent": "agent_8", "session": "run_2", "person": "user_a", "at": 1_002},
        ]);
        let a = hosted_note(json!({"chain": three}));
        assert_eq!(bound(&a, "org_1"), Err("requester".into()));
    }

    #[test]
    fn workspace_sender_cannot_bind_a_note_without_an_agent_hop() {
        let chain = json!([
            {"agent": "agent_1", "session": null, "person": "user_a", "at": 1_000},
            {"agent": null, "session": "run_1", "person": "user_a", "at": 1_001},
        ]);
        let a = hosted_note(json!({"chain": chain}));
        assert_eq!(bound(&a, "org_1"), Err("requester".into()));
    }
}
