# Server-based Lightbulb experiment

This is a clean implementation for the governance experiment. The previous CLI is a reference, not a runtime dependency. There is no P2P networking, replicated member roster, app builder, embedding model, or phone integration in this server.

## Components

- `src/a2a.rs`: official Rust SDK HTTP/JSON-RPC boundary, discovery, bearer identity, task operations.
- `src/main.rs`: local gateway/supplier service composition, authenticated owners, task coordinator, proposal validation, execution permits, thin CLI.
- `src/store.rs`: separate SQLite business ledgers per service, persisted task/approval/audit records.
- `src/policy.rs`: actual Dogwood durable owner/task monitors; no model decides policy.
- `policies/`: typed event schema and temporal policy source.
- `src/agents.rs`: four isolated role invocations; scripted fixture runner and optional constrained live-model calls. No autonomous planner or generic shell access is claimed.
- `web/index.html`: observed events, exact-term approvals and recovery controls.

## Boundaries

Gateway and supplier are distinct localhost processes with separate stores. A2A requests cross HTTP between them. Each business has two logical agent roles. Buyer and supplier approvals use separate credentials; the local demonstration operator is intentionally authorized to control both simulated owners. This is functional separation, not hostile same-host process sandboxing.

A2A provides task/message semantics. Typed business actions and approval scope are application contracts. No proprietary authority extension is required for standard task clients. Signed execution permits are internal gateway-to-supplier application data, not a new A2A transport.

Model outputs never create authenticated owner identity or approval history. Structured proposal validation rejects missing or malformed fields. JSON numbers are canonicalized after the protocol's protobuf Struct conversion; hashes bind normalized, sorted fields. Execution permits bind the task, exact proposal, expiry and supplier reset epoch.

Dogwood policies are isolated per owner/task and frozen for that task. Amount limits are per order, not aggregate budgets. Dogwood response events come from the host after actual successful execution. Human denial revokes exact-proposal authority and ends the task. Unrelated tasks cannot borrow approval history.

## Recovery

The gateway persists dispatch state before contacting the supplier. Supplier order IDs are deterministic per business task and duplicate commits reuse the durable order. An uncertain response produces `unknown`, never a fabricated success. Explicit retry or gateway restart first reads the supplier ledger and reconciles matching terms. Reset rotates the supplier epoch so old signed permits cannot recreate cleared orders.

SQLite and Dogwood stores are separate durable systems, not a distributed transaction. This PoC checks important recovery windows but does not claim production exactly-once behavior across arbitrary downstream systems. Configure authentication, hosting, private audit access, rate/resource limits, policy migrations, and service isolation before any public deployment.

## Verification performed

- Rust tests execute actual Dogwood policies and official Rust A2A HTTP transport.
- Behavioral HTTP tests launch real gateway and supplier processes and inspect both ledgers.
- Independent official Python SDK exercises wire interoperability and caller-scoped task access.
- Browser tests use the real API and inspect desktop/mobile presentation.
- Live-provider calls require user-configured credentials and model selection; none were available during the initial build.
