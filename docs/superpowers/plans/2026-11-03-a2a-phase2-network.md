# A2A phase 2: the network — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every envelope between two computers can carry an A2A Message and a signature; the receiving daemon verifies both, binds the phase-1 authority object to the envelope, judges every envelope by the workspace rules, holds for its person when the rules say so, publishes a signed Agent Card per member, and reads Workers' standing rows as rules.

**Architecture:** Two new modules (`src/authority.rs` verifies the issuer-signed authority object and binds it to an envelope; `src/rules.rs` holds the issuer-signed rules document, the shipped default, local exceptions, and the `decide` port) sit beside the existing mail path. `mail::receive` gains three steps between its kind check and its store: verify `sig`, verify and bind `a2a.metadata[URI]`, decide. A hold is a new `holds::Kind::Authority` row whose message also sits in the mailbox; the person's answer advances the sender's receipt and releases or refuses the row. Cards ride the roster as `Kind::Meta` bodies; the rules document rides the route link as a new op. Every wire and schema change is additive.

**Tech Stack:** Rust (edition as the crate), serde/serde_json, ed25519-dalek, data-encoding, sha2 (all already in `Cargo.toml`), rusqlite, the existing two-daemon test harness (`tests/common`), the debug test issuer (`tests/common/issuer.rs`, `proof::TEST_ISSUER`).

**Spec:** `docs/superpowers/specs/2026-11-02-a2a-phase2-network-design.md` (approved by the founder 2 Nov; its "Clarifications" block is part of it). The monorepo half (spec §8) has its own plan: `docs/superpowers/plans/2026-11-03-a2a-phase2-bridge.md` in the monorepo, on a branch stacked on `feat/a2a-phase1b`.

## Global Constraints

- No `PROTOCOL` bump (`src/net/mod.rs:45` stays `1`). No new `Frame` variant. Every new frame field is `#[serde(default, skip_serializing_if = ...)]`, the pattern `meta` uses (`src/net/frames.rs:201-216`). Old builds must read every new envelope: text stays in `body`.
- Schema changes only through `ADDED_COLUMNS` and `ADDED_TABLES` (`src/store.rs:316-333`); `MIGRATIONS` stays at V15.
- No new crate. Hashing with `sha2`, signing/verifying with `ed25519-dalek` and `data_encoding::BASE64URL_NOPAD`, canonical JSON with `identity::canonical` (`src/identity.rs:232`).
- Exact strings: extension URI `https://thelightbulb.company/a2a/ext/authority/v1`; authority `typ` `lightbulb-a2a-authority`; rules `typ` `lightbulb-a2a-rules`; binding URI `https://thelightbulb.company/a2a/binding/p2p/v1`; card `supportedInterfaces[0].url` `lightbulb://<short>`; refusal reasons reuse `bad_request`, `policy`, `declined`, `no_holder`, `unsupported` (no new `REASONS` entry; `no_holder` is new text on the existing `policy` refusal: use `policy` on the wire and `no_holder` only in the local record).
- Authority verifier limits: `typ == lightbulb-a2a-authority`, `exp - iat <= 86_400`, token `<= 16 * 1024` bytes, `iat <= now + 300` (the proof's `IAT_SKEW_S`), the phase-1 claim shape (`v == 1`, `id`, `requester{person, org, verified_by}`, `chain[]` of `{agent, session, person, at}`, `operation{skill, resource, operands, operands_hash}`, `intent`, optional `on_behalf_of`, `policy` null), `chain[0].person == requester.person`, `operands_hash == "sha256:" + hex(sha256(canonical(operands)))`.
- Binding (spec §4 and Clarifications): `operation.resource` equals `session:<to>` for a `message`, or `worker:<short>/<harness>` for an `assign` (the `to` with `:new` stripped); `operands.text == body`; for an assign the bound operands are `text`, `done_when`, `deadline_at` (as sent), `repo`, `branch`, absent fields omitted; `requester.person` equals the sender's proof `sub`, except when the sender relays a hosted chain (`chain.len() == 2` and the sender's proof `sub == chain[1].person`).
- Rules document claims `{typ, workspace, revision, doc, iat}`; `doc` is the phase-1 shape `{revision, holders{resource: [person | "@session-person"]}, rules[{id, resource, skill, requesters, effect, when?}], default}` with glob `*` only as a whole segment or suffix (`session:*`, `worker:<me>/*`, `agent:*`). A stored document with a newer `iat` replaces; an older one is ignored.
- Decision order (spec §7): outside workspace → deny; any matching deny (rule or local deny-exception) wins; holder rule (a verified person requesting a resource they hold → allow, `holder`); local allow-exception → allow; first matching rule; `default`. The shipped default document: `{"revision": "default", "holders": {"session:*": ["@session-person"]}, "rules": [{"id": "members-message", "resource": "session:*", "skill": "message", "requesters": ["org:*"], "effect": "allow"}], "default": "hold"}`.
- Hold lifetime 24 h (`86_400`), receipt state `Expired` on expiry. An agent session never answers an Authority hold (`NOT_A_PERSON`).
- Tests: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` (CONTRIBUTING). Always with `CARGO_TARGET_DIR=/Users/prasiddhaparthsarthy/Desktop/lightbulb-network.nosync/target-a2a` (one shared target; the disk has ~8 GB free; run `df -h /` before a full `cargo test`). One worktree, never `git stash`, never check out another branch. Never print a token or key.
- Workers: Task 9 touches `src/workers.rs` and the assign carrier. It is dispatched only after the Workers session (or the founder) has answered the coordination message (spec §9).

## Review Focus

1. `a2a.parts` text and `body` differ only by a trailing newline or CRLF → refused `bad_request`, never normalized into acceptance (Task 1).
2. An authority object whose `iat` is up to 300 s ahead of this clock → accepted; 301 s → refused `bad_request` (Task 2).
3. A `rules.set` arriving while an Authority hold is open → the hold stays open and answerable; its answer uses the record the person saw, not the new document (Task 6).
4. A sender with no sign-in proof (old build or not logged in) sending a plain `message` → allowed by the shipped default's `members-message` rule (requester `member:<key>` matches `org:*` only when the sender is a roster member) ; the same sender's `a2a` ask → held with no requester person, record shows `requester: member:<key>` (Task 5).
5. A hold answered `Allow` after the sender's outbox gave up (24 h TTL) → the mailbox row still delivers to the session and the record is written; the sender simply never learns (Task 6).

---

### Task 1: Signed envelopes carrying an A2A body

**Files:**
- Modify: `src/net/frames.rs:181-216` (`Frame::Envelope` gains `a2a: Option<Value>`, `sig: Option<String>`)
- Modify: `src/mail.rs:487-563` (`Prepared` gains `a2a: Option<Value>`, `sig: Option<String>`, `signed_at: Option<i64>`; `From<&Prepared> for Frame` and `Message::from(&Prepared)` carry them), `src/mail.rs:972-1082` (`receive`: verify `sig`, check `body == parts`, refuse `a2a` without `sig`), `src/mail.rs:858-905` (`put`/`record` persist the new columns)
- Modify: `src/outbox.rs:21-31` (`enqueue` signs the frame before storing it)
- Modify: `src/store.rs:316-323` (`ADDED_COLUMNS` += `("messages", "a2a", "TEXT")`, `("messages", "sig", "TEXT")`, `("messages", "signed_at", "INTEGER")`, `("messages", "authority_id", "TEXT")`), `src/store.rs:221-235` (`Message` gains the four optional fields; `row_message` reads them)
- Test: `tests/a2a_envelope.rs` (new; copy the harness of `tests/envelope.rs:42-134`)

**Interfaces:**
- Consumes: `identity::sign(&Value) -> String` and `identity::canonical` (`src/identity.rs:93, 232`); `Member.key` (the peer's member key, base64url Ed25519) from `roster::Member`.
- Produces: `pub fn envelope_sig(id: &Identity, frame: &Value) -> String` and `pub fn verify_envelope_sig(member_key: &str, frame: &Value) -> bool` in `src/net/frames.rs` (both over `canonical(frame without "sig")`, the raw received JSON value); `Prepared { a2a, sig, signed_at, .. }`; `Message { a2a, sig, signed_at, authority_id, .. }`; the receive-path hook point: `receive` takes a new parameter `raw: Option<&Value>` (the frame as received, for signature verification; `None` from local callers).
- `pub fn a2a_text(a2a: &Value) -> Option<String>`: the `parts[].text` joined by `"\n"`, `None` when any part lacks `text` or `role` is not `ROLE_USER`/`ROLE_AGENT`.

- [ ] **Step 1: Write the failing tests** in `tests/a2a_envelope.rs`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signed_envelope_with_a2a_is_stored_with_sig_and_signed_at()   // B sends A a signed envelope with a2a {role ROLE_USER, parts [{text "hi"}]}; A acks stored; A's messages row has a2a, sig, signed_at == the sent_at B signed
async fn tampered_body_is_refused_bad_request()                         // sign, then change body → ack refused bad_request, no row
async fn bad_signature_is_refused_bad_request()                         // sig from another identity
async fn a2a_without_sig_is_refused_bad_request()
async fn body_differing_from_parts_is_refused_bad_request()             // parts text "hi\n", body "hi" (Review Focus 1)
async fn old_format_envelope_without_a2a_is_still_accepted()            // the existing shape, no sig: stored
async fn non_text_part_is_refused_unsupported()                         // parts [{file: ...}]
```

- [ ] **Step 2: Run** `cargo test --test a2a_envelope` → compile error (fields missing).
- [ ] **Step 3: Implement** the fields, `envelope_sig`/`verify_envelope_sig`, `a2a_text`, the three receive checks (after the `from`-prefix check at `mail.rs:1004`, before `check_kind`), the columns, and signing in `outbox::enqueue` (serialize `Frame::from(&p)` to `Value`, set `sig`, store that JSON; `Prepared.sig` is set so the local copy records it). Local sends (`mail::send`) that never leave the computer carry no `sig`.
- [ ] **Step 4: Run** `cargo test --test a2a_envelope --test envelope --test mail_ipc` → PASS; `cargo clippy --all-targets -- -D warnings` clean.
- [ ] **Step 5: Commit** `feat(net): envelopes carry an A2A body and the sender's signature`

---

### Task 2: The authority verifier and envelope binding

**Files:**
- Create: `src/authority.rs` (+ `pub mod authority;` in `src/lib.rs`)
- Modify: `src/proof.rs:39` (`fn issuer_key` → `pub(crate) fn issuer_key`)
- Modify: `src/mail.rs` `receive` (after Task 1's checks: if `a2a.metadata[URI]` is present, `authority::verify` then `authority::bind`, else continue; store `authority_id`)
- Test: unit tests in `src/authority.rs` (`#[cfg(test)] #[path = "../tests/common/issuer.rs"] mod issuer;` as `proof.rs` does), plus two cases appended to `tests/a2a_envelope.rs`

**Interfaces:**
- Consumes: `proof::issuer_key(kid) -> Option<[u8; 32]>`; `proof::Proof { sub, .. }`; `identity::canonical`.
- Produces in `src/authority.rs`: `pub const URI: &str`; `pub const TYP: &str`; `pub const MAX_LIFE_S: i64 = 86_400`; `pub const MAX_TOKEN: usize = 16 * 1024`; `pub struct Authority { pub id: String, pub requester_person: String, pub requester_org: String, pub verified_by: String, pub chain: Vec<Hop>, pub skill: String, pub resource: String, pub operands: Value, pub intent: String, pub iat: i64, pub exp: i64, pub raw: String }` with `pub struct Hop { pub agent: Option<String>, pub session: Option<String>, pub person: String, pub at: i64 }`; `pub fn verify(jws: &str, now: i64) -> Result<Authority>` (errors are `&'static str` naming the check: `size`, `jws`, `alg`, `kid`, `signature`, `typ`, `shape`, `iat`, `exp`, `origin`, `operands_hash`); `pub fn operands_hash(operands: &Value) -> String`; `pub fn bind(a: &Authority, kind: &str, to: &str, body: &str, meta: &mail::Meta, sender_person: Option<&str>) -> Result<()>` (errors `resource`, `operands`, `requester`); `pub fn expected_operands(kind: &str, body: &str, meta: &mail::Meta) -> Value` (the assign operands per Global Constraints).
- `receive` gains the parameter `sender_person: Option<&str>` (the verified `sub` of the peer's hello proof, resolved by the caller in `src/net/mod.rs` from `d.proofs` with `proof::verify(token, &peer.key, now)`; `None` when the peer sent no valid proof).

- [ ] **Step 1: Write the failing unit tests** in `src/authority.rs`:

```rust
fn mint(over: Value) -> String  // helper: issuer::mint of a full claim set with typ, iat 1_000, exp 1_000 + 3_600, overrides merged
#[test] fn verifies_a_well_formed_object()                 // id, requester_person, resource, operands_hash all read back
#[test] fn rejects_wrong_typ_expired_long_life_and_bad_sig()  // four asserts naming typ, exp, exp, signature
#[test] fn accepts_iat_300s_ahead_and_rejects_301()        // Review Focus 2
#[test] fn rejects_origin_mismatch_and_operands_hash()     // chain[0].person != requester.person → origin; wrong hash → operands_hash
#[test] fn bind_checks_resource_text_and_requester()       // session:<to> ok; another to → resource; body differs → operands; sender_person differs → requester; relayed chain of two with sender == chain[1].person → Ok
#[test] fn bind_assign_uses_worker_resource_and_meta_operands()  // kind assign, to "abc/claude:new" → resource worker:abc/claude; operands {text, done_when, deadline_at, repo, branch}
```

- [ ] **Step 2: Run** `cargo test --lib authority` → FAIL (module missing).
- [ ] **Step 3: Implement** `src/authority.rs`; wire `receive` (an `a2a.metadata[URI]` that fails `verify` or `bind` → `refused("bad_request")`; on success `Prepared.authority_id = Some(a.id)`); the caller in `src/net/mod.rs` resolves `sender_person`.
- [ ] **Step 4: Append to `tests/a2a_envelope.rs`:** `async fn envelope_with_valid_authority_is_stored_with_authority_id()` (B's hello carries `issuer::proof(&b.key, "user_b", now)`; the object's requester is `user_b`, resource `session:<A's session>`, operands `{text: body}`) and `async fn authority_requester_not_the_sender_is_refused()`. Run the file → PASS.
- [ ] **Step 5: Commit** `feat(a2a): verify the issuer-signed authority object and bind it to the envelope`

---

### Task 3: The rules module: document, verification, exceptions, decide

**Files:**
- Create: `src/rules.rs` (+ `pub mod rules;`)
- Modify: `src/store.rs:325-333` (`ADDED_TABLES` += `CREATE TABLE IF NOT EXISTS rules (workspace TEXT PRIMARY KEY, revision TEXT NOT NULL, iat INTEGER NOT NULL, jws TEXT NOT NULL, doc TEXT NOT NULL);` and `ADDED_COLUMNS` += `("approvals", "resource", "TEXT")`, `("approvals", "skill", "TEXT")`, `("approvals", "requester", "TEXT")`, `("approvals", "operands_hash", "TEXT")`)
- Test: unit tests in `src/rules.rs`

**Interfaces:**
- Consumes: `proof::issuer_key`; `store::Store` (`conn()`).
- Produces: `pub struct Rules { pub revision: String, pub holders: BTreeMap<String, Vec<String>>, pub rules: Vec<Rule>, pub default: Effect }`, `pub struct Rule { pub id: String, pub resource: String, pub skill: String, pub requesters: Vec<String>, pub effect: Effect, pub when: Option<When> }`, `pub struct When { pub operands_equal: Option<BTreeMap<String, String>>, pub intent_matches: Option<String>, pub verified_by_in: Option<Vec<String>> }` (serde `rename_all = "camelCase"`, `alias` for snake_case), `pub enum Effect { Allow, Deny, Hold }`; `pub fn default_rules() -> Rules`; `pub fn verify_doc(jws: &str, now: i64) -> Result<(String /*workspace*/, i64 /*iat*/, Rules)>` (typ `lightbulb-a2a-rules`, issuer key, `iat <= now + 300`, no exp); `pub fn store_doc(store: &Store, jws: &str, now: i64) -> Result<bool>` (false when an equal-or-newer `iat` is stored); `pub fn load(store: &Store) -> Result<(Option<String>, Rules)>` (the stored document or the default); `pub struct Exception { pub resource: String, pub skill: String, pub requester: String, pub operands_hash: Option<String>, pub deny: bool }`; `pub fn exceptions(store: &Store) -> Result<Vec<Exception>>` (rows with `hold_id IS NULL`: new-style rows by their columns; Workers' legacy rows (`resource IS NULL`, `json_extract(record,'$.member')` set) mapped to `resource: "worker:<me>/*"`, `skill: "assign"`, `requester: "agent:<member short>"`, `deny = scope == "refused"`, skipping `revoked` and any member with a later `revoked`); `pub struct Input<'a> { pub org: Option<&'a str>, pub requester: &'a str /* "person:<sub>" | "member:<key>" */, pub requester_org: Option<&'a str>, pub chain_agents: &'a [String], pub resource: &'a str, pub skill: &'a str, pub session_person: Option<&'a str>, pub operands: Option<&'a Value>, pub intent: Option<&'a str>, pub verified_by: Option<&'a str>, pub operands_hash: Option<&'a str> }`; `pub struct Decision { pub effect: Effect, pub rule: String, pub holders: Vec<String> }`; `pub fn decide(rules: &Rules, exceptions: &[Exception], input: &Input) -> Decision`; `pub fn glob(pattern: &str, value: &str) -> bool`.
- Requester spelling: a verified person is `person:<sub>`; an unverified sender is `member:<key>`; rule `requesters` entries are `person:<sub>`, `agent:<id>`, `org:*` (any requester with `requester_org == org`, or a roster member when `org` is unknown), `*`.

- [ ] **Step 1: Write the failing unit tests** in `src/rules.rs`: `default_document_allows_member_messages_and_holds_the_rest`, `deny_wins_over_exception_and_holder`, `holder_rule_allows_a_person_holding_the_resource`, `allow_exception_matches_resource_skill_requester_and_hash`, `legacy_worker_rows_read_as_assign_exceptions` (an `approvals` row whose `record` is `{"member": "abc…", ...}` with scope `always` → Exception `{worker:<me>/*, assign, agent:abc…, None, false}`; `refused` → deny; `revoked` after `always` → absent), `when_clauses_and_glob`, `verify_doc_rejects_wrong_typ_other_issuer_and_stale_iat`, `store_doc_keeps_the_newest_iat`.
- [ ] **Step 2: Run** `cargo test --lib rules` → FAIL.
- [ ] **Step 3: Implement** `src/rules.rs` (`decide` is a port of the backend's `a2a_rules.decide`, same order) and the schema additions.
- [ ] **Step 4: Run** `cargo test --lib rules store` → PASS; clippy clean.
- [ ] **Step 5: Commit** `feat(rules): the workspace rules document, local exceptions and decide`

---

### Task 4: Rules distribution over the route link

**Files:**
- Modify: `src/link.rs:30-37` (`OPS` += `"rules.set"`), `src/link.rs:238-262` (`answer`: `rules.set {jws}` → `rules::store_doc`, result `{stored: bool, revision}`; refused `bad_request` when verification fails)
- Modify: `src/daemon/ipc.rs` (local op `rules.get` → the stored `{workspace, revision, iat}` or `{revision: "default"}`, person-only: so the app and tests can read it)
- Test: `tests/a2a_rules_link.rs` (new; drive `link::answer` the way `tests/link.rs` drives ops)

**Interfaces:**
- Consumes: `rules::verify_doc`, `rules::store_doc`, `rules::load`.
- Produces: the `rules.set` op and the local `rules.get` op. Ruling (spec §11 leaves the pull to the builder): no `rules.get` over the link; the backend pushes `rules.set` on every link connect and on every rules save (monorepo plan Task A), which covers "on daemon start and after login".

- [ ] **Step 1: Write the failing tests:** `rules_set_from_the_test_issuer_is_stored_and_read_back`, `rules_set_from_another_key_is_refused`, `older_iat_is_ignored`, `rules_set_is_unsupported_on_a_build_without_it` is NOT testable here (old builds), skip.
- [ ] **Step 2: Run** → FAIL. — [ ] **Step 3: Implement.** — [ ] **Step 4: Run** `cargo test --test a2a_rules_link --test link` → PASS.
- [ ] **Step 5: Commit** `feat(link): the backend pushes the signed rules document`

---

### Task 5: Every envelope is judged; allow, deny, and the Authority hold row

**Files:**
- Modify: `src/holds.rs:26-40` (`Kind::Authority`, `as_str` `"authority"`; `Hold::kind()` maps a message whose `authority_id.is_some()` or whose `kind == "message"` with an open hold created by this task → `Authority`: use a new `holds.kind` column rather than inference: `ADDED_COLUMNS` += `("holds", "kind", "TEXT")`, read in `get`/`open`, written by `insert_kind`), `src/holds.rs:88-104` (`pub fn insert_kind(store, message, member, kind: Kind, now) -> Result<Hold>`; `insert` keeps its signature and writes `kind` from the message as before)
- Modify: `src/mail.rs` `receive` (after `check_kind`/declared/members/loop checks and the roster check at `:1078`, before `Prepared`: build `rules::Input`, `rules::decide`; `Deny` → `refused("policy")`; `Hold` with no holders → `refused("policy")` and a local `eprintln` naming `no_holder`; `Hold` → store the message with a mailbox row, `State::Held`, insert the Authority hold, return `Answer { taken: None, held: Some(hold_id), .. }`; `Allow` → as today), `src/mail.rs:33-47` (`Answer.held: Option<i64>`)
- Modify: `src/net/mod.rs` (the receive caller: resolve `sender_person`, `sender_org` (the proof's `org` claim if present), `my_person` (this computer's own proof `sub` via `login::stored_proof` + `proof::peek`), `rules::load`, `rules::exceptions`; pass them in a new `mail::Judge<'_>` struct; on `Answer.held` show the dialog (Task 6) )
- Test: `tests/a2a_hold.rs` (new, two daemons as `tests/holds.rs:51-70` builds them)

**Interfaces:**
- Consumes: Task 2's `Authority` on `Prepared` (keep the verified object in `Prepared.authority: Option<Authority>` for the inputs), Task 3's `rules::*`.
- Produces: `pub struct Judge<'a> { pub sender_person: Option<&'a str>, pub sender_org: Option<&'a str>, pub my_person: Option<&'a str>, pub rules: &'a rules::Rules, pub exceptions: &'a [rules::Exception], pub workspace: Option<&'a str> }` as a `receive` parameter (replacing Task 2's bare `sender_person`); `Answer.held`; `holds::Kind::Authority`; a `messages` row in the mailbox with receipt `Held`.
- Input mapping: `requester` = `person:<authority.requester_person>` when an authority object is bound, else `person:<sender_person>` when the proof verified, else `member:<peer.key>`; `skill` = `authority.skill` or the envelope `kind` (`message`, `assign`); `resource` = `session:<to>` or `worker:<short>/<harness>`; `chain_agents` = the agents in `authority.chain`; `operands`/`intent`/`verified_by` from the object or `None`; `org` = `Judge.workspace` (the stored document's), `requester_org` = `authority.requester_org` or `sender_org` or (roster member → the workspace).

- [ ] **Step 1: Write the failing tests** in `tests/a2a_hold.rs`:

```rust
async fn plain_member_message_with_no_document_is_allowed()            // Review Focus 4a
async fn a2a_ask_with_no_document_is_held_for_the_session_person()      // B's a2a ask to A's session → ack held; A's holds has one open Authority hold; the mailbox holds the row (A's session does not receive it yet)
async fn deny_rule_refuses_policy()                                     // rules.set with a deny on session:* for person:user_b → ack refused policy
async fn hold_with_no_holder_is_refused_policy()                        // A not signed in (no own proof), document holders session:* → @session-person
async fn unsigned_sender_a2a_ask_is_held_as_member()                    // Review Focus 4b: no hello proof, record requester member:<key>
```

- [ ] **Step 2: Run** `cargo test --test a2a_hold` → FAIL. — [ ] **Step 3: Implement.** — [ ] **Step 4: Run** `cargo test --test a2a_hold --test a2a_envelope --test holds --test envelope --test workers` → PASS (workers unchanged: the default document allows member messages and `assign` keeps its own path until Task 9).
- [ ] **Step 5: Commit** `feat(mail): every envelope is judged by the workspace rules; a hold parks it for the person`

---

### Task 6: Answering an Authority hold: dialog, record, release, exception, expiry

**Files:**
- Modify: `src/holds.rs:192-260` (`dialog`: an Authority hold shows `"<requester chain> asks <session> for <person>: <intent>\n\n<request text preview>"`, buttons `["Decline", "Allow"]`, plus `"Always allow"` only when `me`'s own proof person holds the resource under the stored document), `src/holds.rs:278-346` (`answer`: Authority → `signed_record` fields += `authority_id`, `authority_hash` (`sha256:` of the raw JWS), `resource`, `skill`, `requester`; on allow: `mail::advance(.., State::Stored, ..)` and `daemon::deliver` of the mailbox row; on decline: remove the mailbox row, `advance(.., State::Refused, Some("declined"), ..)`; `scope == "always"` writes an exception row with the new columns `(resource, skill, requester, operands_hash NULL)` instead of `workers::set_policy`), `src/holds.rs:356-400` (`expire`: an Authority hold past 24 h closes `expired`, mailbox row removed, receipt `Expired`)
- Modify: `src/net/mod.rs` (on `Answer.held`, queue the dialog as Worker holds do; `by_agent` answers → `NOT_A_PERSON`)
- Test: append to `tests/a2a_hold.rs`

**Interfaces:**
- Consumes: Task 5's hold row and mailbox row; `dialog::Queue::show`; `holds::answer` (`src/holds.rs:278`); `daemon::deliver` (`src/daemon/mod.rs:351`).
- Produces: the answered hold's `approvals` record with `authority_id`, `authority_hash`; receipts `Stored`/`Refused(declined)`/`Expired`.

- [ ] **Step 1: Write the failing tests:**

```rust
async fn allow_delivers_the_held_message_and_writes_the_record_with_authority_id()  // A's session then receives it; approvals record has authority_id == the object's id and authority_hash; B's receipt moves held → stored
async fn decline_refuses_declined()                                                   // B's receipt refused/declined; mailbox empty
async fn always_allow_writes_an_exception_and_the_next_identical_ask_flows()          // second ask acks stored directly; record rule "exception"
async fn an_agent_session_cannot_answer_an_authority_hold()                           // hold.answer with by_agent → not_a_person
async fn authority_hold_expires_after_24h_with_receipt_expired()                      // clock moved: state expired, B's receipt expired
async fn rules_set_while_a_hold_is_open_leaves_it_answerable()                        // Review Focus 3
async fn allow_after_the_senders_outbox_expired_still_delivers()                      // Review Focus 5
```

- [ ] **Step 2: Run** → FAIL. — [ ] **Step 3: Implement.** — [ ] **Step 4: Run** `cargo test --test a2a_hold --test holds --test workers` → PASS.
- [ ] **Step 5: Commit** `feat(holds): a person answers an authority hold; the record carries the authority id`

---

### Task 7: Agent Cards over the roster, the `card` op, `directory`, and the binding page

**Files:**
- Modify: `src/roster.rs:130-150, 455-466` (`Roster.cards: BTreeMap<String, Value>` filled from `Kind::Meta` bodies with a `card` object, at most `MAX_ENTRY_BYTES`; the signer's latest wins)
- Modify: the place this daemon publishes its own `Kind::Meta` entries (find it: `grep -rn 'Kind::Meta' src/daemon src/link.rs src/cloud` ; the `wake` publisher) → publish `{card: <this member's card>}` on start when the roster's card for `me` differs from the one computed now
- Modify: `src/daemon/ipc.rs` (op `card {member?}` → that member's card, default `me`; person or agent caller), `src/presence.rs:640-700` (`directory` rows for members gain `card`)
- Create: `docs/a2a-binding.md` (spec §5: data model mapping from §4, streaming unsupported, error mapping from every `mail::REASONS` entry and `bad_request` to A2A error codes)
- Test: `tests/a2a_cards.rs`

**Interfaces:**
- Consumes: `roster::Entry::signed(id, parents, Kind::Meta, body, at)` (`src/roster.rs:48`); `MAX_ENTRY_BYTES` (`:19`); `workers::offers(config)` for the `assign` skill.
- Produces: `pub fn own_card(short: &str, name: &str, offers_workers: bool) -> Value` in `src/roster.rs` (the exact JSON in spec §5, `skills` with `assign` only when `offers_workers`); the `card` op; `directory` rows' `card`.

- [ ] **Step 1: Write the failing tests:** `own_card_has_the_required_a2a_fields` (unit: `supportedInterfaces[0].url == "lightbulb://<short>"`, `protocolBinding` == the binding URI, `capabilities.extensions[0].uri == authority::URI` with `required: true`, every skill has `id, name, description, tags`), `a_members_card_appears_in_the_roster_and_directory` (two daemons: B's card readable from A via `card {member: B}` and in A's `directory`), `a_card_over_16_kib_is_rejected` (an oversized Meta entry does not replace the card).
- [ ] **Step 2: Run** → FAIL. — [ ] **Step 3: Implement**, and write `docs/a2a-binding.md` (every `REASONS` entry listed; map `bad_request`/`unsupported` → `-32600`/`-32602`, `policy`/`declined`/`not_member`/`not_a_member` → `-32603` with `reason`, `loop` → `-32000`, a hold → task state `TASK_STATE_AUTH_REQUIRED`).
- [ ] **Step 4: Run** `cargo test --test a2a_cards --test directory --test team_cli` → PASS.
- [ ] **Step 5: Commit** `feat(roster): every member publishes a signed Agent Card; the p2p binding page`

---

### Task 8: Sending with authority: `send`/`send_as`, the MCP tool, and the inbox header

**Files:**
- Modify: `src/daemon/ipc.rs:1900-1940` (`send` and `send_as` accept `a2a: Option<Value>`; when present its `metadata[URI]` is kept as given and `parts` are set from `text` if absent), `src/mail.rs:930` (`send` carries `a2a` into `Prepared`)
- Modify: `src/mcp.rs:819` (`send_message` gains optional `authority: String` (a compact JWS) → `a2a: {role: ROLE_AGENT, parts: [{text}], metadata: {URI: authority}, extensions: [URI]}`), `src/frame.rs:111-150` (`framed_kind` takes `authority: Option<&Summary>`; an authority summary line `[A2A] <requester chain> for <person>: <intent>` precedes the text, at most 512 bytes, newlines collapsed)
- Test: `tests/a2a_send.rs` (new; the harness of `tests/mail_ipc.rs`)

**Interfaces:**
- Consumes: Task 1's `Prepared.a2a`; Task 2's `Authority` (for the summary: `pub struct Summary { pub requester: String, pub for_whom: String, pub intent: String }` built in `src/authority.rs`).
- Produces: the `a2a` argument on `send`/`send_as`; the `authority` argument on `send_message`; `frame::framed_kind(kind, label, request_id, reply_to, meta, authority: Option<&authority::Summary>, text)`.

- [ ] **Step 1: Write the failing tests:** `send_with_a2a_signs_and_carries_the_object` (ipc `send` with `a2a` → the peer receives `a2a` and `sig`), `inbox_shows_the_authority_summary_before_the_text` (a held-then-allowed or allowed message with an authority object: the session's inbox text starts with `[A2A] `), `summary_is_capped_at_512_bytes_and_one_line`.
- [ ] **Step 2: Run** → FAIL. — [ ] **Step 3: Implement.** — [ ] **Step 4: Run** `cargo test --test a2a_send --test mail_ipc --test mcp_tools` → PASS.
- [ ] **Step 5: Commit** `feat(send): callers attach an authority object; inboxes show its summary`

---

### Task 9: Workers rows as rules and the assign carrier (GATED on the Workers coordination reply)

**Files:**
- Modify: `src/workers.rs:43-65` (`policy` reads through `rules::exceptions` + `rules::decide` with `resource: worker:<me>/<provider>`, `skill: assign`, `requester: agent:<member short>`; `member == me → Allow` stays first), `src/workers.rs:130-164` (`take_assign` maps `Effect::Allow/Hold/Deny` to `Start`/`Held`/`Refused("declined")`; when the assign carries an authority object, its `bind` (Task 2) already ran in `receive`)
- Modify: `src/mcp.rs` (`assign` tool gains optional `authority`, as `send_message` did)
- Test: `tests/workers.rs` (append; the existing suite must pass unchanged except for the reader)

**Interfaces:**
- Consumes: Task 3's `exceptions`/`decide`; Task 2's `bind` for `assign`.
- Produces: nothing new; `workers::Policy` keeps its three values.

- [ ] **Step 0: Gate.** Do not dispatch until the ledger records the Workers session's (or the founder's) answer to the coordination message describing: the `a2a` field beside `Meta` on an assign envelope, the resource `worker:<short>/<harness>`, the bound operands, and the reader mapping of legacy rows.
- [ ] **Step 1: Write the failing tests:** `assign_with_no_standing_rows_asks_as_today`, `an_always_row_allows`, `a_deny_rule_on_worker_star_refuses_policy`, `assign_with_authority_binds_meta_operands` (a wrong `deadline_at` in the object → `bad_request`).
- [ ] **Step 2: Run** → FAIL. — [ ] **Step 3: Implement.** — [ ] **Step 4: Run** `cargo test --test workers --test a2a_hold` → PASS.
- [ ] **Step 5: Commit** `feat(workers): standing rows read as rules; assignments carry authority`

---

## Not in this plan
Spec §8 (the cloud member bridge and the backend's dispatch) is the monorepo plan. Refusing unsigned envelopes (phase 2b). The engine release, the Hermes roll and `ISSUER_KEYS` are the founder's (spec §9).
