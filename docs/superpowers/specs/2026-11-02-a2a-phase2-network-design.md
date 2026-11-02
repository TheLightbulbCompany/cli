# A2A authority extension, phase 2: the network

**Status:** draft for the founder's approval. Nothing is built.
**Repo:** this one (the CLI engine), branch `feat/a2a-phase2` from `origin/main` at v0.9.7 (4f039af). One monorepo section (§8) names the backend and Hermes changes that go in a sibling branch there.
**Builds on:** phase 1 (monorepo PR #4562): the authority object, the rules document, the signed record, the hosted door. Phase 1b (PR #4571) is independent of this phase.
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
- The person is bound to a computer by the issuer's proof in `Hello.proof` (`proof.rs`, `ISSUER_KEYS` empty, `LIGHTBULB_ISSUER_KEY` env). The authority object from phase 1 is a compact EdDSA JWS by the same issuer key, so `proof.rs`'s key resolution verifies it.
- Hold machinery exists: `holds` and `approvals` tables, `Kind::Message` (dead), `file_permission`, `hold.answer`, `signed_record`, the dialog queue and the app's `held` event (`holds.rs`, `dialog.rs`).
- Workers: `workers::policy` reads standing `approvals` rows (`workers.rs:43-65`); `take_assign` maps Allow/Ask/Refuse (`:130-164`). Workers are on main; the Workers session keeps testing them and sees the format change before it lands (§9).
- Tests run two real daemons in one process tree (`tests/common/mod.rs`, `tests/workers.rs` `Pair`); CONTRIBUTING requires fmt, clippy `-D warnings`, `cargo test`. Disk fills from Rust targets; one worktree, no `git stash`.
- No new crate for JSON canonicalization: `identity::canonical` is the canonical form for everything signed on the network; the A2A Message in the envelope is canonicalized the same way (not RFC 8785; the hosted door's Agent Card signer uses the SDK's JCS, which is a different object and never crosses this boundary).

## 4. The envelope as an A2A Message

`Frame::Envelope` and `mail::Prepared` gain two optional fields:
- `a2a: Option<Value>`: the A2A Message object minus what the envelope already carries: `{"parts": [...], "metadata": {...}, "extensions": [...]}`. `request_id` is the Message id, `thread` is `contextId`, `reply_to` is `referenceTaskIds[0]`, `body` is the text of the text parts joined by newlines. When `a2a` is present the receiver checks `body == join(text parts)` and refuses `bad_request` otherwise, so an old build that ignores `a2a` still reads the same text.
- `sig: Option<String>`: `identity::sign` by the sending computer over the canonical envelope without `sig` (and without `files`, always empty), including `sent_at` as the sender stamped it.

Receive (`mail::receive`, between the `from`-prefix check at `mail.rs:1004` and the kind checks): if `sig` is present, verify against the peer's key or refuse `bad_request`; keep the sender's `sent_at` as `messages.signed_at` (the arrival `now` still goes to `sent_at`, `mail.rs:1093`). If `a2a.metadata[URI]` is present, verify the authority JWS with `proof::issuer_key` (same checks as the hosted door: typ, kid, exp, shape, operands hash, chain origin) or refuse `bad_request`; store the claims. Envelopes with `a2a` but no `sig` are refused. Envelopes without `a2a` are accepted as today (phase 2b refuses them with a protocol bump).

Store: `messages.a2a` and `messages.sig`, `messages.signed_at` through `ADDED_COLUMNS`. `INBOX_RECORD` and `frame::framed_kind` surface the authority summary (requester, for whom, intent) the way the hosted door's 512-byte header does, before the text.

Send: `ipc::send` and `send_as` accept an optional `a2a` object from the caller (the app, the hosted door through the cloud member, a session's MCP `send_message` with an `authority` argument); the daemon signs every outgoing envelope from this release on.

## 5. Agent Card per member

A member's card is a signed roster `Kind::Meta` body (`roster.rs`, the same carrier `Wake` uses), at most 16 KiB, hash-linked and replicated to every member with the roster:
`{"card": {"name", "description", "version": "1", "supportedInterfaces": [{"url": "lightbulb://<short>", "protocolBinding": "LIGHTBULB-P2P", "protocolVersion": "1.0"}], "capabilities": {"streaming": false, "pushNotifications": false, "extensions": [{"uri": URI, "required": true}]}, "defaultInputModes": ["text/plain"], "defaultOutputModes": ["text/plain"], "skills": [{"id": "message"}, {"id": "ask_session"}, {"id": "assign"} when workers are offered]}}`.
A new op `card` (person or agent) returns a member's card; `directory` records gain `card` so the hosted server and the app see it. The binding `LIGHTBULB-P2P` is documented in `docs/a2a-binding.md` per A2A §12: data model mapping (§4 above), streaming unsupported, errors mapped to `REASONS`.

## 6. The hold as the local AUTH_REQUIRED state

The rules document (§7) decides every inbound envelope that carries `a2a`: `allow` stores and delivers as today; `deny` refuses `policy` (receipt `Refused`); `hold` inserts a `Kind::Message` hold (the live precedent is `Taken::Held`, `workers.rs:161-163`), acks `Held`, shows the dialog with the requester chain, intent and operands, buttons `Decline` / `Allow` (and `Always allow` for a person who holds the resource, which writes a local exception row). `hold.answer` writes the signed `approvals` record as today, then advances the receipt to `Stored` and delivers, or `Refused("declined")`. `HOLD_S` expiry closes it `timed_out`. This revives the path the 26 Oct ruling closed ("no message holds"); the founder's phase list reopens it, and §12 asks him to confirm. Who may answer: the person at the keyboard (socket peer with no agent above), the app through `hold.answer`, or the bridge for an instance; an agent session never answers a message hold (`not_a_person`, as for Worker holds).

The signed record is the local half of the phase-1 record; it carries `hold`, `verdict`, `scope`, `by`, `decided_at`, `member`, `text`, `nonce`, `sig`, plus (new) `authority_id` and `authority_hash` so the hosted server can join the two halves.

## 7. Rules on the daemon, and Workers

A `rules` table (`ADDED_TABLES`) holds one workspace rules document, the same JSON as phase 1 (`revision`, `holders`, `rules[]`, `default`), plus `signer` and `received_at`. It arrives as a signed roster `Kind::Meta` body from the cloud member (`{"rules": {...}}`), which is the workspace's authority on the network; a document signed by any other key is ignored. Evaluation is a port of `a2a_rules.decide`: outside-workspace requester → deny; any matching deny wins; a local exception row (the `approvals` rows with `hold_id IS NULL`, re-keyed on resource, skill, requester and operands hash) → allow; first match; default. Resources: `session:<id>`, `computer:<short>`, `worker:<short>/<harness>`; skills: `message`, `ask_session`, `assign`, `cancel`.

Workers: `workers::policy` becomes a call into the same evaluation with resource `worker:<short>/<harness>` and skill `assign`; the existing `always`/`refused`/`revoked` rows are read as local exception rows of that resource (no data migration: the reader changes); the dialog's `Always allow` keeps writing an `always` row. `take_assign`'s three outcomes map to allow, hold, deny. The assign envelope carries the authority object in `a2a.metadata[URI]` (the slot already promised to the Workers session); `Meta` stays as is.

## 8. The cloud member carries hosted sends (monorepo side)

Phase 1 sends a hosted agent's ask to a laptop through the origin person's computer link (`network_route.app_call`). Phase 2 replaces that with the workspace's own member: the backend posts to the Hermes bridge (`apps/infra/hermes/bridge/server.py`, the process that owns the `lightbulb member` socket), a new internal endpoint `POST /a2a/send` with `{to, text, request_id, a2a}`; the bridge calls `app.send` on the member socket; the cloud member signs the envelope with its key and the laptop verifies the envelope signature (cloud member key) and the authority JWS (issuer key). The backend keeps `best_link` as the fallback when the workspace has no cloud member. Replies from the laptop session flow back as network messages to the declared `{app}-{person}` session, which the bridge surfaces to the backend as a task completion (closing the phase-1 gap "a session's reply does not come back").

## 9. Compatibility, release, coordination

- Every change is an additive optional field or an added column or table. Old builds read new envelopes (text in `body`), ignore `a2a`, `sig` and cards, and keep working. No `PROTOCOL` bump.
- The engine release (`docs/releasing.md`: Cargo version, tag, workflow) and the Hermes roll are the founder's. `ISSUER_KEYS` must be filled from `GET /api/v1/network/issuer` before laptops can verify authority objects offline (today only `LIGHTBULB_ISSUER_KEY` works); that is an owner step named in the release notes.
- Workers coordination: the exact `assign` format (the `a2a` field beside `Meta`) and the policy reader change go to the Workers session through the orchestrator before the task that touches `workers.rs` is dispatched, and that task waits for their reply.

## 10. Tests (acceptance)

1. Envelope round trip: a signed envelope with `a2a` is accepted, stored with `a2a`, `sig`, `signed_at`; a tampered body, a bad signature, `a2a` without `sig`, or `body != parts` is refused `bad_request`; an old-format envelope without `a2a` is accepted.
2. Authority: a message with an authority JWS from the test issuer is accepted; expired, wrong typ, mismatched operands hash, or chain origin ≠ requester is refused.
3. Rules and hold, two daemons: B holds a rules document with default `hold` and holders `session:* → @session-person`; A's message to B's session is held, B's person sees the dialog, `Allow` delivers it and writes the record with `authority_id`; `Decline` refuses `declined`; an `always` exception makes the second identical message flow; a deny rule refuses `policy`.
4. Workers: with no standing rows, `assign` behaves as today (Ask); an `always` row allows; a deny rule on `worker:*` refuses; the existing `tests/workers.rs` suite passes unchanged except for the reader.
5. Cards: a member's card appears in the roster and in `directory`; a card over 16 KiB is rejected.
6. Rules distribution: a rules Meta body signed by the cloud member is stored; one signed by another member is ignored.
7. Monorepo: the bridge endpoint forwards a hosted send through the member socket (mocked socket), and the backend prefers it over `best_link` when present.

## 11. Left to the builder

The exact `a2a` JSON shape for non-text parts (refuse them in phase 2); where `signed_at` is surfaced; the `card` op's output shape; the exception-row key migration in `workers::policy` (reader only); the bridge endpoint's auth (the existing bridge token); the dialog text layout for the authority summary; clippy-clean serde attributes.

## 12. Questions for the founder

1. This revives message holds, which the 26 Oct ruling closed ("an invite is the approval; no message holds"). Your phase list reopens them with rules deciding. Confirm.
2. Unsigned envelopes from older builds stay accepted in this phase; refusing them is a later protocol bump. Confirm, or ask for the bump now (every computer must update together).
3. The rules document is distributed as a signed roster entry by the cloud member, so the workspace's hosted member is the authority on every laptop. Acceptable?
4. Hosted sends move from the origin person's computer link to the cloud member. Keep the person-link path as a fallback, or remove it?
5. `ISSUER_KEYS` is empty in the code; filling it from the prod issuer endpoint is an owner step at release time. Who does it, and when?
