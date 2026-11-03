//! The workspace's A2A rules: an issuer-signed document (`verify_doc`, `store_doc`, `load`), this
//! computer's local exceptions (`exceptions`), and `decide`, a port of the backend's
//! `a2a_rules.decide` (`apps/backend/core/services/a2a_rules.py`). No model in the loop.

use crate::store::Store;
use crate::Result;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const TYP: &str = "lightbulb-a2a-rules";
/// A rules document past this is refused unread.
pub const MAX_DOC: usize = 64 * 1024;
/// How far ahead of our clock the issuer may stamp `iat` (the proof's skew).
const IAT_SKEW_S: i64 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Deny,
    Hold,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct When {
    #[serde(alias = "operands_equal")]
    pub operands_equal: Option<BTreeMap<String, String>>,
    #[serde(alias = "intent_matches")]
    pub intent_matches: Option<String>,
    #[serde(alias = "verified_by_in")]
    pub verified_by_in: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    pub id: String,
    pub resource: String,
    pub skill: String,
    pub requesters: Vec<String>,
    pub effect: Effect,
    #[serde(default)]
    pub when: Option<When>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rules {
    #[serde(default)]
    pub revision: String,
    pub holders: BTreeMap<String, Vec<String>>,
    pub rules: Vec<Rule>,
    #[serde(default = "hold")]
    pub default: Effect,
}

fn hold() -> Effect {
    Effect::Hold
}

/// The document used when none is stored: members may message sessions, everything else holds.
pub fn default_rules() -> Rules {
    Rules {
        revision: "default".into(),
        holders: BTreeMap::from([("session:*".into(), vec!["@session-person".into()])]),
        rules: vec![Rule {
            id: "members-message".into(),
            resource: "session:*".into(),
            skill: "message".into(),
            requesters: vec!["org:*".into()],
            effect: Effect::Allow,
            when: None,
        }],
        default: Effect::Hold,
    }
}

/// `(workspace, iat, rules)` of `jws` when it is a rules document from a known issuer key whose
/// `iat` is at most `IAT_SKEW_S` ahead of `now`. It does not expire. Errors name the check.
pub fn verify_doc(jws: &str, now: i64) -> Result<(String, i64, Rules)> {
    let c = crate::authority::verify_jws(jws, MAX_DOC, TYP, now)?;
    let iat = c["iat"].as_i64().ok_or("shape")?;
    if iat > now + IAT_SKEW_S {
        return Err("iat".into());
    }
    let workspace = c["workspace"]
        .as_str()
        .filter(|w| !w.is_empty())
        .ok_or("shape")?;
    let revision = c["revision"].as_str().ok_or("shape")?;
    let mut rules: Rules = serde_json::from_value(c["doc"].clone()).map_err(|_| "shape")?;
    rules.revision = revision.into();
    Ok((workspace.into(), iat, rules))
}

/// Stores `jws` when it verifies, is for this computer's workspace (the first stored document
/// fixes it; another is refused `workspace`) and its `iat` is newer than the stored one.
/// `false` when an equal-or-newer document is already stored.
pub fn store_doc(store: &Store, jws: &str, now: i64) -> Result<bool> {
    let (workspace, iat, rules) = verify_doc(jws, now)?;
    let pinned = store
        .conn()
        .query_row("SELECT workspace FROM rules LIMIT 1", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?;
    if pinned.is_some_and(|p| p != workspace) {
        return Err("workspace".into());
    }
    let changed = store.conn().execute(
        "INSERT INTO rules (workspace, revision, iat, jws, doc) VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(workspace) DO UPDATE SET revision = excluded.revision, iat = excluded.iat, \
         jws = excluded.jws, doc = excluded.doc WHERE excluded.iat > rules.iat",
        rusqlite::params![
            workspace,
            rules.revision,
            iat,
            jws,
            serde_json::to_string(&rules)?
        ],
    )?;
    Ok(changed > 0)
}

/// The stored document and its workspace, or `(None, default_rules())` when none is stored.
/// Fails closed: a database error or a document that does not parse is an error.
pub fn load(store: &Store) -> Result<(Option<String>, Rules)> {
    let row: Option<(String, String)> = store
        .conn()
        .query_row("SELECT workspace, doc FROM rules LIMIT 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    match row {
        Some((w, doc)) => Ok((Some(w), serde_json::from_str(&doc)?)),
        None => Ok((None, default_rules())),
    }
}

/// `(workspace, revision, iat)` of the stored document, if any.
pub fn stored(store: &Store) -> Result<Option<(String, String, i64)>> {
    Ok(store
        .conn()
        .query_row(
            "SELECT workspace, revision, iat FROM rules LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?)
}

/// Forgets the stored document and the workspace it pinned (sign-out).
pub fn clear(store: &Store) -> Result<()> {
    store.conn().execute("DELETE FROM rules", [])?;
    Ok(())
}

/// A standing choice of this computer's person, from an `approvals` row with no hold.
#[derive(Clone, Debug, PartialEq)]
pub struct Exception {
    pub resource: String,
    pub skill: String,
    pub requester: String,
    pub operands_hash: Option<String>,
    pub deny: bool,
}

/// The local exceptions, oldest first. A row without an `operands_hash` is a standing policy
/// (`always` allows, `refused` denies, `revoked` removes the earlier one for the same
/// resource, skill and requester); a row with one is a one-shot allow. Workers' rows (no
/// `resource`, `$.member` in the record) read as `worker:<me>/*`, `assign`, `agent:<member>`;
/// the latest per member wins. Nothing is rewritten on disk.
pub fn exceptions(store: &Store, me: &str) -> Result<Vec<Exception>> {
    let mut q = store.conn().prepare(
        "SELECT scope, resource, skill, requester, operands_hash, json_extract(record, '$.member') \
         FROM approvals WHERE hold_id IS NULL ORDER BY rowid",
    )?;
    let rows = q.query_map([], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?.unwrap_or_default(),
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
        ))
    })?;
    let mut out: Vec<Exception> = vec![];
    for row in rows {
        let (scope, resource, skill, requester, hash, member) = row?;
        let e = match (resource, skill, requester, member) {
            (Some(resource), Some(skill), Some(requester), _) => Exception {
                resource,
                skill,
                requester,
                operands_hash: hash,
                deny: scope == "refused",
            },
            (None, _, _, Some(member))
                if matches!(scope.as_str(), "always" | "refused" | "revoked") =>
            {
                Exception {
                    resource: format!("worker:{me}/*"),
                    skill: "assign".into(),
                    requester: format!("agent:{member}"),
                    operands_hash: None,
                    deny: scope == "refused",
                }
            }
            _ => continue,
        };
        if e.operands_hash.is_none() {
            if !matches!(scope.as_str(), "always" | "refused" | "revoked") {
                continue;
            }
            out.retain(|o| {
                o.operands_hash.is_some()
                    || (&o.resource, &o.skill, &o.requester)
                        != (&e.resource, &e.skill, &e.requester)
            });
            if scope == "revoked" {
                continue;
            }
        }
        out.push(e);
    }
    Ok(out)
}

/// What `decide` judges. `requester` is `person:<sub>` (verified) or `member:<key>`;
/// `requester_org` is the sender's proof org, or `org` itself for a roster member without one.
pub struct Input<'a> {
    pub org: Option<&'a str>,
    pub requester: &'a str,
    pub requester_org: Option<&'a str>,
    pub chain_agents: &'a [String],
    pub resource: &'a str,
    pub skill: &'a str,
    pub session_person: Option<&'a str>,
    pub operands: Option<&'a Value>,
    pub intent: Option<&'a str>,
    pub verified_by: Option<&'a str>,
    pub operands_hash: Option<&'a str>,
    /// Whether the authority object's `verified_by` is a person source; an agent-originated
    /// ask is never upgraded by the holder rule.
    pub origin_is_person: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    pub effect: Effect,
    pub rule: String,
    pub holders: Vec<String>,
}

/// `*` alone matches anything; a trailing `*` matches any value starting with the text before
/// it; otherwise the two must be equal.
pub fn glob(pattern: &str, value: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => value.starts_with(prefix),
        None => pattern == value,
    }
}

/// `to_lowercase` stands in for the backend's `casefold` (std has none; ß-class characters may miss a match).
fn norm(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Every set clause must match; a set clause with no input never does.
fn when_matches(w: &Option<When>, i: &Input) -> bool {
    let Some(w) = w else { return true };
    if let Some(eq) = &w.operands_equal {
        let ok = i.operands.is_some_and(|o| {
            eq.iter()
                .all(|(k, v)| o.get(k).and_then(Value::as_str) == Some(v.as_str()))
        });
        if !ok {
            return false;
        }
    }
    if let Some(m) = &w.intent_matches {
        if i.intent.is_none_or(|t| norm(t) != norm(m)) {
            return false;
        }
    }
    w.verified_by_in
        .as_ref()
        .is_none_or(|v| i.verified_by.is_some_and(|b| v.iter().any(|x| x == b)))
}

/// The holders of the best-matching key (an exact key, else the longest wildcard prefix), with
/// `@session-person` replaced by the session's person, or dropped when there is none.
fn holders_of(rules: &Rules, i: &Input) -> Vec<String> {
    let best = rules
        .holders
        .keys()
        .filter(|k| glob(k, i.resource))
        .max_by_key(|k| (!k.ends_with('*'), k.len()));
    let Some(best) = best else { return vec![] };
    rules.holders[best]
        .iter()
        .filter_map(|h| match h.as_str() {
            "@session-person" => i.session_person.map(String::from),
            h => Some(h.into()),
        })
        .collect()
}

fn requester_matches(r: &str, i: &Input) -> bool {
    match r {
        "*" => true,
        "org:*" => i
            .requester_org
            .is_some_and(|o| i.org.is_none_or(|w| w == o)),
        _ => {
            r == i.requester
                || r.strip_prefix("org:")
                    .is_some_and(|o| i.requester_org == Some(o))
                || r.strip_prefix("agent:")
                    .is_some_and(|a| i.chain_agents.iter().any(|c| c == a))
        }
    }
}

/// The decision, in order: outside the workspace denies; any matching deny (rule or exception)
/// wins; a local allow-exception allows; else the first matching rule, else `default`; a
/// `hold` becomes an allow (`holder`) for a verified person who holds the resource.
pub fn decide(rules: &Rules, exceptions: &[Exception], i: &Input) -> Decision {
    let holders = holders_of(rules, i);
    let done = |effect, rule: &str| Decision {
        effect,
        rule: rule.into(),
        holders: holders.clone(),
    };
    if let (Some(w), Some(o)) = (i.org, i.requester_org) {
        if w != o {
            return Decision {
                effect: Effect::Deny,
                rule: "outside_org".into(),
                holders: vec![],
            };
        }
    }
    let hits: Vec<&Rule> = rules
        .rules
        .iter()
        .filter(|r| {
            glob(&r.resource, i.resource)
                && r.skill == i.skill
                && r.requesters.iter().any(|q| requester_matches(q, i))
                && when_matches(&r.when, i)
        })
        .collect();
    if let Some(r) = hits.iter().find(|r| r.effect == Effect::Deny) {
        return done(Effect::Deny, &r.id);
    }
    let matching: Vec<&Exception> = exceptions
        .iter()
        .filter(|e| {
            glob(&e.resource, i.resource)
                && e.skill == i.skill
                && requester_matches(&e.requester, i)
                && e.operands_hash
                    .as_deref()
                    .is_none_or(|h| i.operands_hash == Some(h))
        })
        .collect();
    if matching.iter().any(|e| e.deny) {
        return done(Effect::Deny, "exception:deny");
    }
    if !matching.is_empty() {
        return done(Effect::Allow, "exception:allow");
    }
    let (effect, rule) = hits
        .first()
        .map_or((rules.default, "default"), |r| (r.effect, &r.id));
    if effect == Effect::Hold && i.origin_is_person {
        if let Some(sub) = i.requester.strip_prefix("person:") {
            if holders.iter().any(|h| h == sub) {
                return done(Effect::Allow, "holder");
            }
        }
    }
    done(effect, rule)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proof::test_issuer as issuer;
    use serde_json::json;

    const NOW: i64 = 1_800_000_000;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("state.db")).unwrap();
        (dir, s)
    }

    fn doc(rule: Value) -> Value {
        json!({"revision": "x", "holders": {"session:*": ["@session-person"]},
               "rules": [rule], "default": "hold"})
    }

    fn mint(workspace: &str, iat: i64, revision: &str) -> String {
        let claims = json!({"workspace": workspace, "revision": revision, "iat": iat,
            "doc": doc(json!({"id": "r1", "resource": "session:*", "skill": "message",
                              "requesters": ["org:*"], "effect": "allow"}))});
        issuer::mint_typ(TYP, &claims)
    }

    fn input<'a>(requester: &'a str, resource: &'a str, skill: &'a str) -> Input<'a> {
        Input {
            org: Some("org_1"),
            requester,
            requester_org: Some("org_1"),
            chain_agents: &[],
            resource,
            skill,
            session_person: Some("user_a"),
            operands: None,
            intent: None,
            verified_by: None,
            operands_hash: None,
            origin_is_person: true,
        }
    }

    fn rule(id: &str, resource: &str, skill: &str, requesters: &[&str], effect: Effect) -> Rule {
        Rule {
            id: id.into(),
            resource: resource.into(),
            skill: skill.into(),
            requesters: requesters.iter().map(|s| s.to_string()).collect(),
            effect,
            when: None,
        }
    }

    fn exc(
        resource: &str,
        skill: &str,
        requester: &str,
        hash: Option<&str>,
        deny: bool,
    ) -> Exception {
        Exception {
            resource: resource.into(),
            skill: skill.into(),
            requester: requester.into(),
            operands_hash: hash.map(String::from),
            deny,
        }
    }

    #[test]
    fn clear_forgets_the_document_and_the_pin() {
        let (_d, s) = store();
        assert!(store_doc(&s, &mint("ws1", NOW, "r1"), NOW).unwrap());
        assert_eq!(stored(&s).unwrap(), Some(("ws1".into(), "r1".into(), NOW)));
        clear(&s).unwrap();
        assert_eq!(stored(&s).unwrap(), None);
        assert!(store_doc(&s, &mint("ws2", NOW, "r2"), NOW).unwrap());
    }

    #[test]
    fn default_document_allows_member_messages_and_holds_the_rest() {
        let r = default_rules();
        let d = decide(
            &r,
            &[],
            &input("member:k1", "session:abc/claude:1", "message"),
        );
        assert_eq!(
            (d.effect, d.rule.as_str()),
            (Effect::Allow, "members-message")
        );
        let d = decide(
            &r,
            &[],
            &input("member:k1", "session:abc/claude:1", "ask_session"),
        );
        assert_eq!((d.effect, d.rule.as_str()), (Effect::Hold, "default"));
        assert_eq!(d.holders, vec!["user_a".to_string()]);
        let d = decide(&r, &[], &input("member:k1", "worker:abc/claude", "assign"));
        assert_eq!(d.effect, Effect::Hold);
        // Not a roster member and no proof: org:* does not match.
        let mut i = input("member:k1", "session:abc/claude:1", "message");
        i.requester_org = None;
        assert_eq!(decide(&r, &[], &i).effect, Effect::Hold);
        // Another workspace.
        i.requester_org = Some("org_2");
        assert_eq!(decide(&r, &[], &i).rule, "outside_org");
    }

    #[test]
    fn deny_wins_over_exception_and_holder() {
        let mut r = default_rules();
        r.rules.push(rule(
            "d1",
            "session:*",
            "ask_session",
            &["person:user_a"],
            Effect::Deny,
        ));
        let i = input("person:user_a", "session:abc/claude:1", "ask_session");
        let allow = exc("session:*", "ask_session", "person:user_a", None, false);
        let d = decide(&r, std::slice::from_ref(&allow), &i);
        assert_eq!((d.effect, d.rule.as_str()), (Effect::Deny, "d1"));
        // A deny exception beats an allow rule and an allow exception.
        let r = default_rules();
        let i = input("person:user_a", "session:abc/claude:1", "message");
        let deny = exc("session:abc/*", "message", "person:user_a", None, true);
        assert_eq!(decide(&r, &[allow, deny], &i).effect, Effect::Deny);
    }

    #[test]
    fn holder_rule_allows_a_person_holding_the_resource() {
        let r = default_rules();
        let i = input("person:user_a", "session:abc/claude:1", "ask_session");
        let d = decide(&r, &[], &i);
        assert_eq!((d.effect, d.rule.as_str()), (Effect::Allow, "holder"));
        // Another person asking is held; a member with no proof too.
        assert_eq!(
            decide(&r, &[], &input("person:user_b", i.resource, i.skill)).effect,
            Effect::Hold
        );
        assert_eq!(
            decide(&r, &[], &input("member:k1", i.resource, i.skill)).effect,
            Effect::Hold
        );
        // A computer with no session person has no holder.
        let mut j = input("person:user_a", i.resource, i.skill);
        j.session_person = None;
        let d = decide(&r, &[], &j);
        assert_eq!((d.effect, d.holders.len()), (Effect::Hold, 0));
        // An agent-originated ask stays held.
        let mut k = input("person:user_a", i.resource, i.skill);
        k.origin_is_person = false;
        assert_eq!(decide(&r, &[], &k).effect, Effect::Hold);
        // A default of deny stays a deny.
        let mut deny = default_rules();
        deny.default = Effect::Deny;
        assert_eq!(decide(&deny, &[], &i).effect, Effect::Deny);
    }

    #[test]
    fn allow_exception_matches_resource_skill_requester_and_hash() {
        let r = default_rules();
        let mut i = input("person:user_b", "session:abc/claude:1", "ask_session");
        i.operands_hash = Some("sha256:aa");
        let standing = exc("session:abc/*", "ask_session", "person:user_b", None, false);
        let d = decide(&r, std::slice::from_ref(&standing), &i);
        assert_eq!(
            (d.effect, d.rule.as_str()),
            (Effect::Allow, "exception:allow")
        );
        let once = exc(
            "session:abc/*",
            "ask_session",
            "person:user_b",
            Some("sha256:aa"),
            false,
        );
        assert_eq!(
            decide(&r, std::slice::from_ref(&once), &i).effect,
            Effect::Allow
        );
        i.operands_hash = Some("sha256:bb");
        assert_eq!(decide(&r, &[once], &i).effect, Effect::Hold, "another hash");
        i.operands_hash = None;
        let other_skill = exc("session:abc/*", "message", "person:user_b", None, false);
        let other_who = exc("session:abc/*", "ask_session", "person:user_c", None, false);
        let other_res = exc("session:zzz/*", "ask_session", "person:user_b", None, false);
        for e in [other_skill, other_who, other_res] {
            assert_eq!(decide(&r, &[e], &i).effect, Effect::Hold);
        }
        // An agent requester matches the chain.
        let agents = ["friday".to_string()];
        let mut j = input("person:user_b", "worker:me/claude", "assign");
        j.chain_agents = &agents;
        let e = exc("worker:me/*", "assign", "agent:friday", None, false);
        assert_eq!(decide(&r, &[e], &j).effect, Effect::Allow);
    }

    fn legacy(s: &Store, scope: &str, member: &str) {
        s.conn()
            .execute(
                "INSERT INTO approvals (hold_id, verdict, scope, \"by\", nonce, decided_at, record) \
                 VALUES (NULL, 'x', ?1, 'me', ?2, 1, ?3)",
                rusqlite::params![
                    scope,
                    format!("{scope}{member}{}", rand_nonce()),
                    json!({"member": member, "scope": scope}).to_string()
                ],
            )
            .unwrap();
    }

    fn rand_nonce() -> u128 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed) as u128
    }

    #[test]
    fn legacy_worker_rows_read_as_assign_exceptions() {
        let (_d, s) = store();
        legacy(&s, "always", "abc");
        legacy(&s, "refused", "def");
        legacy(&s, "always", "ghi");
        legacy(&s, "revoked", "ghi");
        legacy(&s, "refused", "jkl");
        legacy(&s, "always", "jkl");
        let got = exceptions(&s, "me1").unwrap();
        assert_eq!(
            got,
            vec![
                exc("worker:me1/*", "assign", "agent:abc", None, false),
                exc("worker:me1/*", "assign", "agent:def", None, true),
                exc("worker:me1/*", "assign", "agent:jkl", None, false),
            ]
        );
        // A row bound to a hold is not an exception.
        s.conn()
            .execute(
                "INSERT INTO approvals (hold_id, scope, nonce, record) VALUES (7, 'always', 'n9', '{\"member\":\"zzz\"}')",
                [],
            )
            .unwrap();
        assert_eq!(exceptions(&s, "me1").unwrap().len(), 3);
        // New-style rows: standing, revoked, one-shot.
        let new = |scope: &str, hash: Option<&str>, n: &str| {
            s.conn()
                .execute(
                    "INSERT INTO approvals (hold_id, scope, nonce, resource, skill, requester, operands_hash) \
                     VALUES (NULL, ?1, ?2, 'session:a/*', 'ask_session', 'person:u', ?3)",
                    rusqlite::params![scope, n, hash],
                )
                .unwrap();
        };
        new("always", None, "n1");
        new("always", Some("sha256:aa"), "n2");
        assert_eq!(exceptions(&s, "me1").unwrap().len(), 5);
        new("revoked", None, "n3");
        let got = exceptions(&s, "me1").unwrap();
        assert_eq!(got.len(), 4);
        assert!(got
            .iter()
            .any(|e| e.operands_hash.as_deref() == Some("sha256:aa")));
    }

    #[test]
    fn when_clauses_and_glob() {
        assert!(glob("*", "anything"));
        assert!(glob("session:*", "session:abc/claude:1"));
        assert!(glob("worker:abc/*", "worker:abc/claude"));
        assert!(glob("agent:*", "agent:friday"));
        assert!(!glob("session:*", "worker:abc/claude"));
        assert!(!glob("worker:abc/*", "worker:abd/claude"));
        assert!(glob("session:abc", "session:abc"));
        assert!(!glob("session:abc", "session:abcd"));
        assert!(!glob("session:*", "sessions:x"));
        assert!(!glob("session:*", "session"));
        assert!(!glob("a*c", "abc"), "a mid-* pattern is a literal");

        let w = |v: Value| Some(serde_json::from_value::<When>(v).unwrap());
        let mut r = default_rules();
        r.rules = vec![Rule {
            when: w(
                json!({"operandsEqual": {"repo": "r"}, "intent_matches": "Ship  It",
                           "verifiedBy_in": null, "verified_by_in": ["person_source"]}),
            ),
            ..rule("w", "session:*", "message", &["*"], Effect::Allow)
        }];
        let ops = json!({"repo": "r", "text": "x"});
        let mut i = input("member:k", "session:a/c:1", "message");
        i.operands = Some(&ops);
        i.intent = Some("  ship\tit ");
        i.verified_by = Some("person_source");
        assert_eq!(decide(&r, &[], &i).effect, Effect::Allow);
        let bad = json!({"repo": "q"});
        i.operands = Some(&bad);
        assert_eq!(decide(&r, &[], &i).effect, Effect::Hold, "operand differs");
        i.operands = None;
        assert_eq!(
            decide(&r, &[], &i).effect,
            Effect::Hold,
            "no operands never matches"
        );
        i.operands = Some(&ops);
        i.intent = None;
        assert_eq!(
            decide(&r, &[], &i).effect,
            Effect::Hold,
            "no intent never matches"
        );
        i.intent = Some("ship it");
        i.verified_by = Some("other");
        assert_eq!(decide(&r, &[], &i).effect, Effect::Hold, "wrong verifier");
        i.verified_by = None;
        assert_eq!(
            decide(&r, &[], &i).effect,
            Effect::Hold,
            "no verifier never matches"
        );
        // snake_case and camelCase documents both read.
        let a: Rules = serde_json::from_value(json!({"holders": {}, "rules": [
            {"id": "a", "resource": "*", "skill": "s", "requesters": [], "effect": "deny",
             "when": {"operands_equal": {"k": "v"}}}]}))
        .unwrap();
        assert_eq!(a.default, Effect::Hold);
        assert!(a.rules[0].when.as_ref().unwrap().operands_equal.is_some());
    }

    #[test]
    fn verify_doc_rejects_wrong_typ_other_issuer_and_stale_iat() {
        let good = mint("ws1", NOW, "rev7");
        let (w, iat, r) = verify_doc(&good, NOW).unwrap();
        assert_eq!((w.as_str(), iat, r.revision.as_str()), ("ws1", NOW, "rev7"));
        assert_eq!(r.rules[0].id, "r1");
        // iat up to 300 s ahead is fine, 301 s is not.
        assert!(verify_doc(&mint("ws1", NOW + 300, "r"), NOW).is_ok());
        assert_eq!(
            verify_doc(&mint("ws1", NOW + 301, "r"), NOW)
                .unwrap_err()
                .to_string(),
            "iat"
        );
        // An old document does not expire.
        assert!(verify_doc(&mint("ws1", NOW - 100 * 86_400, "r"), NOW).is_ok());
        let claims =
            json!({"workspace": "ws1", "revision": "r", "iat": NOW, "doc": doc(json!({}))});
        let wrong = issuer::mint_typ("lightbulb-a2a-authority", &claims);
        assert_eq!(verify_doc(&wrong, NOW).unwrap_err().to_string(), "typ");
        // `typ` only among the claims is not it.
        let mut c = claims.clone();
        c["typ"] = TYP.into();
        assert_eq!(
            verify_doc(&issuer::mint(&c), NOW).unwrap_err().to_string(),
            "typ"
        );
        // Another issuer's key id, and a tampered signature.
        let (_, rest) = good.split_once('.').unwrap();
        let header = json!({"alg": "EdDSA", "kid": "someone-else", "typ": TYP}).to_string();
        let other = format!(
            "{}.{rest}",
            data_encoding::BASE64URL_NOPAD.encode(header.as_bytes())
        );
        assert_eq!(verify_doc(&other, NOW).unwrap_err().to_string(), "kid");
        let (head, sig) = good.rsplit_once('.').unwrap();
        let flipped = format!("{head}.{}", sig.replacen(&sig[..2], "AA", 1));
        assert!(verify_doc(&flipped, NOW).is_err());
        assert_eq!(
            verify_doc(&"a".repeat(MAX_DOC + 1), NOW)
                .unwrap_err()
                .to_string(),
            "size"
        );
    }

    #[test]
    fn store_doc_keeps_the_newest_iat() {
        let (_d, s) = store();
        let (w, r) = load(&s).unwrap();
        assert_eq!((w, r.revision.as_str()), (None, "default"));
        assert!(store_doc(&s, &mint("ws1", NOW, "a"), NOW).unwrap());
        assert!(
            !store_doc(&s, &mint("ws1", NOW, "same"), NOW).unwrap(),
            "equal iat"
        );
        assert!(
            !store_doc(&s, &mint("ws1", NOW - 1, "old"), NOW).unwrap(),
            "older iat"
        );
        assert_eq!(load(&s).unwrap().1.revision, "a");
        assert!(store_doc(&s, &mint("ws1", NOW + 10, "b"), NOW + 10).unwrap());
        let (w, r) = load(&s).unwrap();
        assert_eq!((w.as_deref(), r.revision.as_str()), (Some("ws1"), "b"));
        assert_eq!(r.rules[0].id, "r1");
        assert!(store_doc(&s, "garbage", NOW).is_err());
    }

    #[test]
    fn store_doc_pins_the_workspace_and_load_fails_closed() {
        let (_d, s) = store();
        assert!(store_doc(&s, &mint("ws1", NOW, "a"), NOW).unwrap());
        let e = store_doc(&s, &mint("ws2", NOW + 5, "b"), NOW + 5).unwrap_err();
        assert_eq!(e.to_string(), "workspace");
        let (w, r) = load(&s).unwrap();
        assert_eq!((w.as_deref(), r.revision.as_str()), (Some("ws1"), "a"));
        assert!(store_doc(&s, &mint("ws1", NOW + 5, "c"), NOW + 5).unwrap());
        assert!(!store_doc(&s, &mint("ws1", NOW + 1, "old"), NOW + 5).unwrap());
        assert_eq!(load(&s).unwrap().1.revision, "c");
        s.conn()
            .execute("UPDATE rules SET doc = '{nope'", [])
            .unwrap();
        assert!(load(&s).is_err());
    }

    #[test]
    fn verify_doc_refuses_a_bad_revision_or_doc_and_stores_nothing() {
        let (_d, s) = store();
        let good = json!({"workspace": "ws1", "iat": NOW, "revision": "r",
            "doc": doc(json!({"id": "r1", "resource": "*", "skill": "s", "requesters": [], "effect": "allow"}))});
        let mut num = good.clone();
        num["revision"] = 7.into();
        let mut bad = good.clone();
        bad["doc"]["rules"][0]["effect"] = "maybe".into();
        for c in [num, bad] {
            let jws = issuer::mint_typ(TYP, &c);
            assert_eq!(verify_doc(&jws, NOW).unwrap_err().to_string(), "shape");
            assert!(store_doc(&s, &jws, NOW).is_err());
        }
        assert_eq!(load(&s).unwrap().0, None);
    }
}
