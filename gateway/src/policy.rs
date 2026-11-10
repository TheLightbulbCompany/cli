//! Dogwood decisions for one owner/task. Limits are PER ORDER, not aggregate
//! budgets. The host authenticates actors and computes immutable proposal hashes.
//! Dogwood stores monitor state; the gateway separately owns approval UI, audit,
//! idempotency and the transaction that actually executes an allowed action.
use dogwood_language::{Event, EventBuilder, Value};
use dogwood_local_engine::{DurableTemporalEngine, Outcome};
use serde::Serialize;
use std::path::Path;

const SCHEMA: &str = include_str!("../policies/actions.cedarschema");
const POLICY: &str = include_str!("../policies/commerce.dw");

#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub allowed: bool,
    pub requires_approval: bool,
    pub reasons: Vec<String>,
}

pub struct PolicyEngine {
    engine: DurableTemporalEngine,
    owner: String,
    task: String,
}

impl PolicyEngine {
    pub fn open(path: &Path, owner: &str, task: &str, max_amount: i64) -> Result<Self, String> {
        if owner.is_empty() || task.is_empty() || max_amount < 0 {
            return Err("owner, task and a nonnegative per-order limit are required".into());
        }
        let mut engine = DurableTemporalEngine::open(path, 1000).map_err(|e| e.to_string())?;
        // Never reinstall on recovery: install replaces policies and resets their
        // temporal history. Bind persisted configuration to this store explicitly.
        let metadata_path = path.with_extension("policy-config.json");
        let policy_source = POLICY.replace("__MAX_AMOUNT__", &max_amount.to_string());
        let expected = serde_json::json!({"owner": owner, "task": task,
            "policy": policy_source, "schema": SCHEMA})
        .to_string();
        if engine.has_policy() {
            let persisted = std::fs::read_to_string(&metadata_path)
                .map_err(|e| format!("policy store configuration missing: {e}"))?;
            if persisted != expected {
                return Err("policy store configuration changed; create a fresh task store".into());
            }
        } else {
            engine
                .install(&policy_source, SCHEMA, None, None)
                .map_err(|e| e.to_string())?;
            std::fs::write(&metadata_path, expected).map_err(|e| e.to_string())?;
        }
        Ok(Self {
            engine,
            owner: owner.into(),
            task: task.into(),
        })
    }

    // Mirrors the typed event schema in one centralized constructor.
    #[allow(clippy::too_many_arguments)]
    fn event(
        &self,
        action: &str,
        kind: &str,
        actor: &str,
        proposal: &str,
        amount: i64,
        substitution: bool,
        confidential: bool,
    ) -> EventBuilder {
        // Use one stable monitor principal within this already isolated policy
        // domain so owner approval responses and agent requests share history.
        // The authenticated actor remains an explicit, host-derived field.
        let mut event = Event::builder(&format!("Lightbulb::Action::{action}"), kind)
            .principal("Lightbulb::Scope::\"task\"")
            .resource("Lightbulb::Resource::\"commerce\"");
        let fields = [
            ("owner", Value::String(self.owner.clone())),
            ("task", Value::String(self.task.clone())),
            ("actor", Value::String(actor.into())),
            ("proposal", Value::String(proposal.into())),
            ("amount", Value::Int(amount)),
            ("substitution", Value::Bool(substitution)),
            ("confidential", Value::Bool(confidential)),
        ];
        for (key, value) in fields {
            event = event
                .field("input", key, value.clone())
                .request_context("input", key, value);
        }
        event
    }

    fn decide(&mut self, event: EventBuilder, eligible: bool) -> Result<Decision, String> {
        let result = self.engine.submit(event).map_err(|e| e.to_string())?;
        let Outcome::Decision(verdict) = result.outcome else {
            return Err("expected a Dogwood decision, got history acknowledgement".into());
        };
        let errors: Vec<String> = verdict.diagnostics().errors().map(str::to_owned).collect();
        if !errors.is_empty() {
            return Err(format!(
                "Dogwood evaluation failed closed: {}",
                errors.join("; ")
            ));
        }
        let allowed = verdict.allowed();
        let reasons = verdict
            .diagnostics()
            .reason()
            .map(|r| r.annotation_id().unwrap_or("unannotated-policy").to_owned())
            .collect();
        Ok(Decision {
            allowed,
            requires_approval: !allowed && eligible,
            reasons,
        })
    }

    pub fn evaluate_commit(
        &mut self,
        proposal_hash: &str,
        amount: i64,
        substitution: bool,
    ) -> Result<Decision, String> {
        if proposal_hash.is_empty() {
            return Err("proposal hash is required".into());
        }
        let event = self.event(
            "CommitOrder",
            "request",
            "agent",
            proposal_hash,
            amount,
            substitution,
            false,
        );
        self.decide(event, amount >= 0)
    }

    /// Supplier discount authority is per proposal: percentages above ten require
    /// this supplier owner's exact-proposal approval. `amount` in the event
    /// schema carries integer percent for Discount, cents for CommitOrder.
    pub fn evaluate_discount(
        &mut self,
        proposal_hash: &str,
        discount_percent: i64,
    ) -> Result<Decision, String> {
        if proposal_hash.is_empty() {
            return Err("proposal hash is required".into());
        }
        let event = self.event(
            "Discount",
            "request",
            "agent",
            proposal_hash,
            discount_percent,
            false,
            false,
        );
        self.decide(event, (0..=100).contains(&discount_percent))
    }

    pub fn evaluate_export(&mut self, confidential: bool) -> Result<Decision, String> {
        let event = self.event("Export", "request", "agent", "", 0, false, confidential);
        self.decide(event, false)
    }

    fn owner_event(&mut self, action: &str, actor: &str, hash: &str) -> Result<(), String> {
        if actor != self.owner || hash.is_empty() {
            return Err(
                "only this task's authenticated owner may approve or revoke a proposal".into(),
            );
        }
        let event = self.event(action, "request", actor, hash, 0, false, false);
        if !self.decide(event, false)?.allowed {
            return Err("owner event denied".into());
        }
        let response = self
            .event(action, "response", actor, hash, 0, false, false)
            .field("output", "success", Value::Bool(true))
            .request_context("output", "success", Value::Bool(true));
        match self
            .engine
            .submit(response)
            .map_err(|e| e.to_string())?
            .outcome
        {
            Outcome::Recorded => Ok(()),
            _ => Err("expected history acknowledgement".into()),
        }
    }
    /// Record the real executor outcome AFTER an authorized commit. A failed
    /// action records an error history event, never a fabricated success.
    pub fn record_commit_result(
        &mut self,
        hash: &str,
        amount: i64,
        substitution: bool,
        success: bool,
    ) -> Result<(), String> {
        let kind = if success { "response" } else { "error" };
        let mut event = self.event(
            "CommitOrder",
            kind,
            "agent",
            hash,
            amount,
            substitution,
            false,
        );
        if success {
            event = event
                .field("output", "success", Value::Bool(true))
                .request_context("output", "success", Value::Bool(true));
        }
        match self
            .engine
            .submit(event)
            .map_err(|e| e.to_string())?
            .outcome
        {
            Outcome::Recorded => Ok(()),
            _ => Err("expected history acknowledgement".into()),
        }
    }

    pub fn approve(&mut self, actor: &str, hash: &str) -> Result<(), String> {
        self.owner_event("Approve", actor, hash)
    }
    pub fn revoke(&mut self, actor: &str, hash: &str) -> Result<(), String> {
        self.owner_event("Revoke", actor, hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn engine(dir: &tempfile::TempDir, file: &str, owner: &str, task: &str) -> PolicyEngine {
        PolicyEngine::open(&dir.path().join(file), owner, task, 50_000).unwrap()
    }
    #[test]
    fn actual_dogwood_exact_approval_and_revocation() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = engine(&dir, "buyer.redb", "buyer", "task1");
        assert!(p.evaluate_commit("a", 49_000, false).unwrap().allowed);
        p.record_commit_result("a", 49_000, false, true).unwrap();
        p.record_commit_result("failed", 49_000, false, false)
            .unwrap();
        assert!(
            p.evaluate_commit("b", 51_000, false)
                .unwrap()
                .requires_approval
        );
        assert!(
            p.evaluate_commit("c", 10_000, true)
                .unwrap()
                .requires_approval
        );
        assert!(p.approve("seller", "b").is_err());
        p.approve("buyer", "b").unwrap();
        assert!(p.evaluate_commit("b", 51_000, false).unwrap().allowed);
        assert!(!p.evaluate_commit("changed", 51_000, false).unwrap().allowed);
        p.revoke("buyer", "b").unwrap();
        assert!(!p.evaluate_commit("b", 51_000, false).unwrap().allowed);
        p.approve("buyer", "b").unwrap();
        assert!(p.evaluate_commit("b", 51_000, false).unwrap().allowed);
        let bad = p.evaluate_commit("negative", -1, false).unwrap();
        assert!(!bad.allowed && !bad.requires_approval);
        let export = p.evaluate_export(true).unwrap();
        assert!(!export.allowed && !export.requires_approval);
        assert!(p.evaluate_export(false).unwrap().allowed);
        assert!(p.evaluate_discount("d", 10).unwrap().allowed);
        assert!(p.evaluate_discount("d", 15).unwrap().requires_approval);
        p.approve("buyer", "d").unwrap();
        assert!(p.evaluate_discount("d", 15).unwrap().allowed);
        p.revoke("buyer", "d").unwrap();
        assert!(!p.evaluate_discount("d", 15).unwrap().allowed);
    }
    #[test]
    fn persisted_approval_and_separate_owner_history() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut p = engine(&dir, "buyer.redb", "buyer", "task1");
            p.approve("buyer", "hash").unwrap();
        }
        let mut p = engine(&dir, "buyer.redb", "buyer", "task1");
        assert!(p.evaluate_commit("hash", 60_000, false).unwrap().allowed);
        let mut other = engine(&dir, "seller.redb", "seller", "task1");
        assert!(
            !other
                .evaluate_commit("hash", 60_000, false)
                .unwrap()
                .allowed
        );
        drop(p);
        assert!(
            PolicyEngine::open(
                &dir.path().join("buyer.redb"),
                "other-owner",
                "task1",
                50_000
            )
            .is_err()
        );
        assert!(
            PolicyEngine::open(&dir.path().join("buyer.redb"), "buyer", "task1", 70_000).is_err()
        );
    }
}
