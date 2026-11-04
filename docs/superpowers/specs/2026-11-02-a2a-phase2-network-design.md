# A2A authority extension, phase 2: the network

**Status:** draft for the founder's approval. Nothing is built.
**Repo:** this one (the CLI engine), branch `feat/a2a-phase2` from `origin/main` at v0.9.7 (4f039af). One monorepo section (§8) names the backend and Hermes changes that go in a sibling branch there.
**Builds on:** phase 1 (monorepo PR #4562): the authority object, the rules document, the signed record, the hosted door. Phase 1b (PR #4571) is independent of this phase.
**Builder check (2 Nov):** run; nine questions and six contradictions folded in; calls marked "my call".
**Scope (the founder's phase list):** P2P envelopes become A2A Messages with signatures and metadata; Agent Cards per member over the roster; the message hold revived as the local AUTH_REQUIRED state; Workers' Allow/Ask/Refuse rows become workspace rules; the cloud member bridge carries hosted sends. Needs an engine release and a Hermes roll, which only the founder triggers.

## 1. Intent

Today a message between two computers is authenticated by the QUIC handshake and gated by membership only (`mail.rs:1066` "No one holds a member's message: membership is the gate"). Nothing says who asked, for whom, or whether the receiving person agreed; the hold for messages is dead code (`holds.rs:293-298`). Phase 1 put all of that behind the hosted door. Phase 2 brings it to the network so that every cross-person message carries the same authority object, is signed by the sending computer, is judged by the same rules document on the receiving computer, and is held for its person when the rules do not decide. The protocol becomes A2A's data model over our transport, as the spec allows for custom bindings (A2A §12).

## 2. Goals, non-goals, anti-goals

Goals: an envelope can carry an A2A Message (parts, metadata, extensions) and a signature; the receiving daemon verifies both and decides by the workspace rules; `hold` becomes a real local state answered by the person with the existing dialog and signed record; every member publishes a signed Agent Card; the backend's hosted sends reach laptops through the cloud member instead of the origin person's link; Workers' standing policy rows become rules in the same document.

Non-goals: a protocol-version bump in this phase; refusing unsigned envelopes from older builds (phase 2b); a new wire frame variant; moving memory, presence or roster to A2A; third-party A2A clients talking to a laptop directly (they talk to the hosted door, which forwards).

Anti-goals: a signature that the receiver does not verify; a rule evaluated by a model; a hold the person cannot see or answer offline; breaking an old build's ability to read new envelopes.

## 3. Constraints (from the code)

- Frames are JSON tagged by `t`, one per message, at most 1 MiB (`frames.rs:1-2,16`). A new `Frame` variant stops an old reader (`frames.rs:181`); unknown fields on a known frame are ignored (no `deny_unknown_fields`). Every addition in this phase is an optional field with `#[serde(default, skip_serializing_if)]`, the pattern `meta` used (`frames.rs:201-216`).
- `PROTOCOL = 1` is exact-match and has never been bumped (`net/mod.rs:43-45`, `1139-1144`). Phase 2 does not bump it.
- Schema changes go through `ADDED_COLUMNS` and `ADDED_TABLES` (`store.rs:311-356`), which an older build can still open; `MIGRATIONS` is at V15.
- Signing: `identity::sign` is Ed25519 over `canonical(v without sig)`, keys sorted recursively (`identity.rs:93-96, 231-233`). Roster entries, memory entries, hold answers, worker policy rows, join frames, share grants and tickets are signed today; envelopes are not (`frames.rs:159-160`: "ponytail: sign envelopes if they are ever forwarded").
- The person is bound to a computer by the issuer's proof in `Hello.proof` (`proof.rs`, `ISSUER_KEYS` empty, `LIGHTBULB_ISSUER_KEY` env). The authority object from phase 1 is a compact EdDSA JWS by the same issuer key. `proof::verify` is proof-only (no `typ` check, `Proof` shape, 4096-byte cap, 31-day life; `proof.rs:16, 69-104`) and `issuer_key` is private (`:39`), so this phase adds `src/authority.rs` with its own verifier that reuses the key resolver: `typ == lightbulb-a2a-authority`, life ≤ 86 400 s, cap 16 KiB, the phase-1 shape and operands-hash checks.
- Hold machinery exists: `holds` and `approvals` tables, `Kind::Message` (dead), `file_permission`, `hold.answer`, `signed_record`, the dialog queue and the app's `held` event (`holds.rs`, `dialog.rs`).
- Workers: `workers::policy` reads standing `approvals` rows (`workers.rs:43-65`); `take_assign` maps Allow/Ask/Refuse (`:130-164`). Workers are on main; the Workers session keeps testing them and sees the format change before it lands (§9).
- Tests run two real daemons in one process tree (`tests/common/mod.rs`, `tests/workers.rs` `Pair`); CONTRIBUTING requires fmt, clippy `-D warnings`, `cargo test`. Disk fills from Rust targets; one worktree, no `git stash`.
- No new crate for JSON canonicalization: `identity::canonical` is the canonical form for everything signed on the network; the A2A Message in the envelope is canonicalized the same way (not RFC 8785; the hosted door's Agent Card signer uses the SDK's JCS, which is a different object and never crosses this boundary).

## 4. The envelope as an A2A Message

`Frame::Envelope` and `mail::Prepared` gain two optional fields (`Prepared` has no serde; the builder wires them through `From<&Prepared> for Frame`, `mail.rs:547-563`):
- `a2a: Option<Value>`: the A2A Message minus what the envelope already carries: `{"role": "ROLE_USER" | "ROLE_AGENT", "parts": [{"text": ...}], "metadata": {...}, "extensions": [...]}`. `request_id` is the Message id, `thread` is `contextId`; `reply_to` stays an envelope field (it is a message id, not a task id). `body` is the text parts joined by newlines; the receiver refuses `bad_request` when they differ, so an old build that ignores `a2a` reads the same text. Non-text parts are refused in this phase.
- `sig: Option<String>`: `identity::sign` by the sending computer over the canonical envelope without `sig`. The signature is computed at `outbox::enqueue` (`outbox.rs:21`), before the frame is stored, so the signed `sent_at` is the sender's stamp; the receiver verifies over the raw received JSON value, never over a re-serialized `Frame`, so fields a later build adds survive (builder-check question 9). Arrival `now` still goes to `messages.sent_at` (`mail.rs:1093`); the sender's stamp is kept as `messages.signed_at`.

Receive (`mail::receive`, after the `from`-prefix check at `mail.rs:1004`): `sig` present → verify against the peer's key or refuse `bad_request`. `a2a` present without `sig` → refuse. `a2a.metadata[URI]` present → verify the authority JWS with `authority::verify` and then bind it to this envelope, or refuse `bad_request` (builder-check question 2): `operation.resource` must equal `session:<to>` (or `computer:<short>`, `worker:<to>` for assigns); `operands.text` must equal `body`; `requester.person` must equal the sending computer's proof `sub` from `Hello.proof`, except when the sender is relaying a hosted chain (the object's `chain` has two entries and the sender's proof `sub` equals `chain[1].person`, which is how the cloud member carries a hosted agent's ask). Envelopes without `a2a` are accepted and are still judged by the rules (§7) with the sender's proof person as requester, skill `message`, no intent.

Store: `messages.a2a`, `messages.sig`, `messages.signed_at`, `messages.authority_id` through `ADDED_COLUMNS`. `INBOX_RECORD` and `frame::framed_kind` surface the authority summary (requester, for whom, intent, at most 512 bytes) before the text, the way the hosted door's header does.

Send: `ipc::send` and `send_as` accept an optional `a2a` object from the caller (the app, the cloud member on behalf of the hosted door, a session's `send_message` with an `authority` argument); every outgoing envelope is signed from this release on.

## 5. Agent Card per member

A member's card is a signed roster `Kind::Meta` body (`roster.rs`, the same carrier `Wake` uses), at most 16 KiB, hash-linked and replicated to every member with the roster:
`{"card": {"name", "description", "version": "1", "supportedInterfaces": [{"url": "lightbulb://<short>", "protocolBinding": "https://thelightbulb.company/a2a/binding/p2p/v1", "protocolVersion": "1.0"}], "capabilities": {"streaming": false, "pushNotifications": false, "extensions": [{"uri": URI, "required": true, "description": "Signed authority on every message."}]}, "defaultInputModes": ["text/plain"], "defaultOutputModes": ["text/plain"], "skills": [{"id": "message", "name": "Message", "description": "Send this computer's sessions a message.", "tags": ["message"]}, {"id": "ask_session", "name": "Ask a session", "description": "Ask one of this computer's sessions to do something.", "tags": ["session"]}, {"id": "assign", "name": "Start a worker", "description": "Start a worker on this computer.", "tags": ["worker"]} when workers are offered]}}` (A2A requires `id`, `name`, `description`, `tags` on a skill and a URI for `protocolBinding`).
A new op `card` (person or agent) returns a member's card; `directory` records gain `card` so the hosted server and the app see it. The binding is documented in `docs/a2a-binding.md` per A2A §12: data model mapping (§4), streaming unsupported, and an error mapping from every `REASONS` entry and `bad_request` to A2A error codes. Cards change rarely, so the roster's permanent hash-linked log is the right carrier for them (not for the rules document, §7).

## 6. The hold as the local AUTH_REQUIRED state

A new hold kind `Kind::Authority` (my call, builder-check question 5): the dead `Kind::Message` stays dead, `expire` keeps closing hub-era rows, and `kind()`'s unknown-to-Message mapping is untouched. The rules decide every inbound envelope (§7): `allow` stores and delivers as today; `deny` refuses `policy`; `hold` stores the message in the mailbox with receipt `Held` (unlike `Taken::Held`, which stores no mailbox row), inserts an Authority hold, acks `Held`, and shows the dialog with the requester chain, intent and the request text, buttons `Decline` / `Allow`, plus `Always allow` only when the answering person holds the resource (it writes a standing local exception, §7). `hold.answer` on an Authority hold writes the signed `approvals` record as today, then advances the receipt to `Stored` and delivers the mailbox row, or `Refused("declined")`. Expiry is 24 h (`HOLD_S`), receipt state `Expired` (an existing `State`; no new `REASONS` entry). Who may answer: the person at the keyboard, the app through `hold.answer`, or the bridge for an instance; an agent session never answers an Authority hold (`not_a_person`, as for Worker holds). This revives the path the 26 Oct ruling closed; §12 asks the founder to confirm.

The signed record is the local half of the phase-1 record: `hold`, `verdict`, `scope`, `by`, `decided_at`, `member`, `text`, `nonce`, `sig`, plus `authority_id` and `authority_hash`, so the hosted server can join the two halves.

## 7. Rules on the daemon, and Workers

Trust anchor (builder-check question 1, my call): the roster has no notion of a cloud member, and any member may sign a Meta body, so the rules document is NOT a roster entry. It is a compact JWS signed by the backend's issuer key, the key every laptop already trusts for sign-in proofs, with claims `{typ: lightbulb-a2a-rules, workspace, revision, doc, iat}`. It reaches a computer two ways: pushed over the route link as a new op `rules.set {jws}` whenever an admin saves rules (`link.rs` `OPS` gains it; the computer verifies and stores), and pulled with `rules.get` on daemon start and after login. A `rules` table (`ADDED_TABLES`) holds the latest verified document per workspace with its `revision`; a document with an older `iat` than the stored one is ignored. No growth in the roster, no ordering by hash.

Evaluation is a port of `a2a_rules.decide` into `src/rules.rs`, run in `mail::receive` between `check_kind` (`mail.rs:1053`) and the store (`:1082`), for EVERY envelope (builder-check question 3): an envelope without `a2a` is judged with requester = the sender's proof person (or the sender's member key when the sender has no proof), skill `message`, resource `session:<to>`, no intent. Inputs the daemon has: `workspace` from the stored document; `requester_org` = the sender's proof `org` claim when present, else "same workspace" when the sender is a roster member; `session_person` = this computer's own proof `sub` (a computer with no sign-in has no session person, so an `@session-person` hold on it is refused `no_holder`); `origin_is_person` = whether the object's `verified_by` is a person source. Order: outside-workspace → deny; any matching deny (rule or local deny-exception) wins; the phase-1 holder rule (a verified person requesting a resource they hold → allow, `holder`); a local allow-exception → allow; first matching rule; default. The shipped default document (used when none is stored) allows `skill: message` from workspace members to `session:*` and holds everything else, so plain member messages keep flowing exactly as today (builder-check question 3).

Local exceptions are the existing `approvals` rows with `hold_id IS NULL`, extended (builder-check question 4): a row carries `resource`, `skill`, `requester` and optionally `operands_hash`; a row without an operands hash is a standing policy (`always`, `refused` → a deny-exception, `revoked` → removed); a row with one is a one-shot allow. Workers' existing rows have only `$.member` and a scope: the reader maps them to `resource: worker:<me>/*`, `skill: assign`, `requester: agent:<member short>`, no operands hash, so nothing is rewritten on disk. `member == me → Allow` stays a built-in. `take_assign`'s three outcomes map to allow, hold, deny; the assign envelope carries the authority object in `a2a.metadata[URI]` (the slot promised to the Workers session); `Meta` stays as is.

## 8. The cloud member carries hosted sends (monorepo side)

Phase 1 sends a hosted agent's ask to a laptop through the origin person's computer link (`network_route.app_call`). Phase 2 adds the workspace's own member as the preferred path. The Hermes bridge serves `/invocations` with RPC methods (`bridge/server.py:543-547`, `RPC_METHODS` `network.*`), and `bridge/network.py` owns the member socket (`ask`/`call`, `network.py:250-275`). So: a new RPC method `network.a2a_send {to, text, request_id, a2a, person}` on the bridge; it first ensures the session `{app: "a2a", person}` is declared through `cloud.sessions.set` (`app.send` refuses `not_declared` otherwise, `app_send.rs:28-45`), then calls `app.send` on the member socket with the `a2a` object; the cloud member signs the envelope with its key; the laptop verifies the envelope signature (cloud member key, a roster member) and the authority JWS (issuer key), and the relay rule in §4 binds the chain. The backend's `_dispatch_session` tries the bridge first and falls back to `best_link` when the workspace has no running cloud member. Replies from the laptop session flow back as network messages to the declared `a2a-<person>` session; the bridge forwards them to the backend as a task completion (closing the phase-1 gap "a session's reply does not come back").

## 9. Compatibility, release, coordination

- Every change is an additive optional field, an added column or table, or a new op on the route link. Old builds read new envelopes (text in `body`), ignore `a2a`, `sig` and cards, refuse `rules.set` as `unsupported`, and keep working. No `PROTOCOL` bump.
- The engine release (`docs/releasing.md`: Cargo version, tag, workflow) and the Hermes roll are the founder's. `ISSUER_KEYS` must be filled from `GET /api/v1/network/issuer` before laptops can verify authority objects offline (today only `LIGHTBULB_ISSUER_KEY` works); that is an owner step named in the release notes.
- Workers coordination: the exact `assign` format (the `a2a` field beside `Meta`) and the policy reader change go to the Workers session through the orchestrator before the task that touches `workers.rs` is dispatched, and that task waits for their reply.

## 10. Tests (acceptance)

1. Envelope round trip: a signed envelope with `a2a` is accepted, stored with `a2a`, `sig`, `signed_at`; a tampered body, a bad signature, `a2a` without `sig`, or `body != parts` is refused `bad_request`; an old-format envelope without `a2a` is accepted.
2. Authority: a message with an authority JWS from the test issuer is accepted; expired, wrong typ, mismatched operands hash, chain origin ≠ requester, `resource` ≠ `session:<to>`, `operands.text` ≠ `body`, or `requester.person` ≠ the sender's proof person (without a relayed chain) is refused.
3. Rules and hold, two daemons: B holds a rules document with default `hold` and holders `session:* → @session-person`; A's message to B's session is held, B's person sees the dialog, `Allow` delivers it and writes the record with `authority_id`; `Decline` refuses `declined`; an `always` exception makes the second identical message flow; a deny rule refuses `policy`.
4. Workers: with no standing rows, `assign` behaves as today (Ask); an `always` row allows; a deny rule on `worker:*` refuses; the existing `tests/workers.rs` suite passes unchanged except for the reader.
5. Cards: a member's card appears in the roster and in `directory`; a card over 16 KiB is rejected.
6. Rules distribution: a `rules.set` JWS signed by the test issuer is stored; one signed by another key is refused; an older `iat` is ignored; a plain member message with no stored document is allowed; an `a2a` ask with no stored document is held.
7. Monorepo: the bridge endpoint forwards a hosted send through the member socket (mocked socket), and the backend prefers it over `best_link` when present.

## 11. Left to the builder

The `src/authority.rs` and `src/rules.rs` module layout; where `signed_at` is surfaced; the `card` op's output shape; the exception-row reader mapping in `workers::policy`; the bridge RPC method's auth (the existing bridge token) and the declared-session bookkeeping; the dialog text layout for the authority summary; clippy-clean serde attributes; the `rules.get` pull on login.

## 12. Questions for the founder

1. This revives message holds, which the 26 Oct ruling closed ("an invite is the approval; no message holds"). Your phase list reopens them with rules deciding. Confirm.
2. Unsigned envelopes from older builds stay accepted in this phase; refusing them is a later protocol bump. Confirm, or ask for the bump now (every computer must update together).
3. The rules document is signed by the backend's issuer key and pushed to each computer over its route link, so Lightbulb's server is the rules authority on every laptop, the same trust every sign-in proof already relies on. Acceptable?
4. Hosted sends move from the origin person's computer link to the cloud member. Keep the person-link path as a fallback, or remove it?
5. `ISSUER_KEYS` is empty in the code; filling it from the prod issuer endpoint is an owner step at release time. Who does it, and when?

## Clarifications (2 Nov, evening; after the super-agent session's questions)

- §4 resource for assigns: `worker:<short>/<harness>`, the envelope's `to` with `:new` stripped (`:new` is addressing, `workers.rs:124`, `provider_of` `:213`). §7's `worker:<me>/*` globs the harness.
- §4 operands for `skill: assign`: `text` (= `body`), `done_when`, `deadline_at` (the integer as sent, before the receiver's cap at `workers.rs:153`), `repo`, `branch` (`Meta`, `mail.rs:470`); absent fields omitted; `operands_hash` as in phase 1.
- Cancel stays the built-in `cancel` kind (`mail.rs:15`), accepted only from the parent assign's own `from_key` (`mail.rs:666`) with its target from the parent (`mail.rs:761`). No rules action and no `cancel` skill in this phase; cancelling someone else's assignment is a later capability.
- A receiver without a card (an old build) still receives hosted envelopes and behaves as today; the backend records `receiver_enforces: false` on the task record. A receiver whose card lists the extension runs the second check. Refusing card-less receivers outright is a founder call, not taken here.
