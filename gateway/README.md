# Lightbulb gateway proof of concept

A local experiment in governing delegation between two independently owned agent systems. A buyer and supplier coordinate a synthetic purchase, while Lightbulb evaluates policies and records approvals at the action boundary.

This is a separate Rust server experiment. It does not remove or modify the P2P transport in [the existing Lightbulb CLI](https://github.com/TheLightbulbCompany/cli/tree/090b549471c8c12d5370dc6b86aa5d34683642f1). Its design builds on that CLI's authority binding, allow/deny/hold decisions, owner approvals, and harness adapters. Code in this experiment should not be described as an already integrated CLI upgrade.

## Run locally

With a recent stable Rust toolchain installed:

```sh
cargo build
```

Start the supplier service in one terminal, using its own state directory:

```sh
cargo run -- --role supplier --port 8081 --data-dir work/supplier
```

Start the gateway in another terminal:

```sh
cargo run -- --role gateway --port 8080 --data-dir work/gateway --supplier-url http://127.0.0.1:8081
```

The default local demo credentials are:

| Role | Environment variable | Default |
| --- | --- | --- |
| Buyer owner | `LIGHTBULB_BUYER_OWNER_TOKEN` | `demo-buyer` |
| Supplier owner | `LIGHTBULB_SUPPLIER_OWNER_TOKEN` | `demo-supplier` |
| Demo operator/admin | `LIGHTBULB_OWNER_TOKEN` | `demo-owner` |
| Gateway-to-supplier API | `LIGHTBULB_SUPPLIER_TOKEN` | `internal-demo` |
| Execution-permit signing secret | `LIGHTBULB_EXECUTION_SECRET` | `local-demo-secret` |

An approval request supplies `X-Owner-Token` for the matching owner. The operator credential intentionally administers both sides in the local lab and resets the demo; it is not evidence that one company's owner can authorize the other company's work. The tests use distinct buyer and supplier credentials and verify that each fails to approve the other's proposal, even when the request claims the correct owner name.

These defaults are conveniences for a loopback-only synthetic lab, not deployable production credentials. Set environment variables consistently for both processes when changing supplier credentials or the execution secret.

Keep both services bound to loopback for this experiment. Open the gateway at `http://127.0.0.1:8080` and inspect its available demo controls. `/health` reports service readiness.

In this Codex workspace the toolchain was installed under `/workspace/work/toolchain`, rather than the home directory. If needed, use:

```sh
export RUSTUP_HOME=/workspace/work/toolchain/rustup
export CARGO_HOME=/workspace/work/toolchain/cargo
export PATH="$CARGO_HOME/bin:$PATH"
cargo build
```

## CLI walkthrough

The same binary can administer a running gateway. The default gateway URL is `http://127.0.0.1:8080`; client commands also accept `--url`.

```sh
target/debug/lightbulb-gateway demo --scenario approval
target/debug/lightbulb-gateway tasks
```

Read the pending approval ID in task state, then approve or decline the exact proposal with its named owner's credential:

```sh
target/debug/lightbulb-gateway approve <approval-id> --owner buyer
target/debug/lightbulb-gateway deny <approval-id> --owner supplier
```

Available scenarios are `automatic`, `approval`, `supplier`, `disclosure`, and `changed_terms`. A changed-terms run intentionally creates a second approval after the first is accepted; the original approval cannot authorize the revised price.

```sh
target/debug/lightbulb-gateway retry <task-id>
target/debug/lightbulb-gateway evidence --output evidence.json
target/debug/lightbulb-gateway reset
```

`retry` reconciles only an unknown execution outcome. `reset` clears both synthetic business ledgers and rotates their reset generation; it refuses while work is active. Evidence is exported from actual recorded state, not generated narration. It can include internal execution permits and conversation data, so inspect and redact it before sharing.

## What the experiment tests

The buyer and supplier have separate authority and separate records. A request allowed by one organization does not automatically authorize the other organization's actions. Decisions should be tied to structured actions and exact terms, rather than an agent's conversational claim that an owner approved something.

The demonstration covers automatic authorization, buyer approval, supplier approval, denied disclosure, and changed terms requiring a fresh decision. Inspect the recorded action and supplier order, not just the message transcript, to establish what actually executed.

## Scope and limits

- Business data, payments, and orders are synthetic. Nothing is purchased from an external merchant.
- Scripted actors establish repeatable policy and transport behavior. If a live model mode is available, it must be explicitly selected and labeled; replayed or scripted runs are not evidence of autonomous model behavior.
- A2A standardizes task exchange. Lightbulb's business action schema and approval records are application semantics layered around it.
- Dogwood issues policy verdicts over the events supplied to it. Lightbulb remains responsible for trustworthy event construction, enforcing verdicts, recording approvals, and preventing duplicate business effects.
- A local demonstration is not a production authentication, tenancy, sandboxing, or availability implementation. Agents must not have an alternate route to protected tools if action enforcement is to mean anything.
- Durable policy-monitor state is not a complete audit log. Task, decision, approval, and execution records must be retained separately.

## Commit recovery experiment

A dispatched purchase whose response is lost is recorded as `unknown`, rather than reported as failed or confirmed. Retrying with the demo operator credential (`POST /api/tasks/{id}/retry`) or restarting the gateway first queries the supplier's order ledger. A confirmed existing order is reused instead of blindly committing a second purchase.

To exercise that path, start the gateway with `LIGHTBULB_INJECT_LOST_COMMIT_RESPONSE=1`. This injects a lost response after the supplier commits, once per task, with an on-disk marker. The HTTP tests verify supplier state before recovery and confirm that restart produces one order with one commit request. This is a synthetic failure-injection mode, not a claim about an external merchant integration.

Supplier resets rotate the synthetic supplier generation. Previously signed execution permits belong to the old generation and cannot recreate cleared orders after a reset.

## Verification

The HTTP integration suite uses only Python's standard library and starts isolated supplier and gateway processes with temporary data directories:

```sh
python3 tests/e2e.py --binary target/debug/lightbulb-gateway
```

Run Rust unit tests separately:

```sh
cargo test
```

The integration suite should verify observable outcomes: no order before approval, no order after denial, correct owner credential authorization, changed terms invalidating old approval, no duplicate order, A2A cancellation, direct supplier permit rejection, and recovery after restart. The suite's passing output is the record of which checks actually ran.

## One-command demonstration

After building, run `./run-demo.sh` to start both localhost services. The source-only launcher uses Cargo; a distribution with a `bin/lightbulb-gateway` executable uses that binary instead. Ctrl-C stops both services. Set `LIGHTBULB_GATEWAY_PORT` and `LIGHTBULB_SUPPLIER_PORT` to choose different ports.

## Optional live model steps

Set `LIGHTBULB_MODEL` and either `OPENAI_API_KEY` or `ANTHROPIC_API_KEY` in both service environments, then choose Live in the viewer or pass `demo --mode live`. Credentials belong in environment settings, not prompts or source files. If both keys exist, OpenAI is selected. Model/endpoint compatibility depends on the selected model; no live-provider run was performed in this initial build.

Live mode consists of four constrained role/tool steps with independent observations, not an autonomous general planner. Every model-proposed action still passes through validation and the same policy boundary. Without credentials, the Live control stays disabled and all scripted tests remain runnable.

## Independent A2A interoperability check

```sh
python3 -m venv work/a2a-client
work/a2a-client/bin/pip install a2a-sdk==1.3.0
work/a2a-client/bin/python tests/a2a_smoke.py --binary target/debug/lightbulb-gateway
```

This uses the official Python SDK against the Rust server and checks discovery, task completion, result decoding, retry deduplication, and caller isolation. It is not a claim of full TCK certification.
