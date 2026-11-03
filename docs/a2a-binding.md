# The Lightbulb peer-to-peer binding for A2A

This page is the custom protocol binding that A2A section 12 asks for. Binding URI:
`https://thelightbulb.company/a2a/binding/p2p/v1`. Every member's Agent Card names it in
`supportedInterfaces[0].protocolBinding`, with the address `lightbulb://<member short>` and
`protocolVersion` `1.0`.

## Transport

A message travels between two computers of one team over an authenticated QUIC connection, as one
JSON frame (`envelope`) of at most 1 MiB. The receiving computer answers with a receipt frame.
There are no HTTP requests, no server-sent events and no push webhooks.

## Agent Cards

Each member signs its own card into the team's roster as a `meta` entry with a `card` object, at
most 16 KiB. The roster is a hash-linked log replicated to every member, so a card reaches the
team with it; a member's latest card wins, and an entry over 16 KiB never replaces one. The
`card` operation on a computer returns a member's card (the member's short or key; default: its
own), and each `directory` record carries the card of the member that owns the session.

The card declares one extension, the signed authority object, with `required: true`. Its skills are
`message` and `ask_session`, plus `assign` when the member's computer offers workers.

## Data model

An envelope is an A2A Message with the transport fields kept beside it:

| Envelope | A2A Message |
| --- | --- |
| `request_id` | `messageId` |
| `thread` | `contextId` |
| `reply_to` | a reference to an earlier message id (not a task id) |
| `body` | the text parts, joined by newlines; the receiver refuses the message when `body` and the parts differ |
| `a2a.role` | `role` (`ROLE_USER` or `ROLE_AGENT`) |
| `a2a.parts` | `parts`; text parts only |
| `a2a.metadata` | `metadata` |
| `a2a.metadata["https://thelightbulb.company/a2a/ext/authority/v1"]` | the authority extension: a signed statement of who asks, for whom, and what |
| `a2a.extensions` | `extensions` |
| `sig` | an Ed25519 signature by the sending computer over the canonical envelope without `sig` |

An envelope without `a2a` is a plain text message and is still accepted. The text of such a
message is `body`.

## What this binding does not do

Streaming is unsupported: `capabilities.streaming` and `capabilities.pushNotifications` are
`false`. A reply is a new message in the same thread, not a stream. Non-text parts are refused.

## Errors

A refusal reaches the sender as a receipt with state `refused` and a `reason`. Each reason maps to
an A2A error as follows.

| Reason | A2A error |
| --- | --- |
| `bad_request` | `-32600` (invalid request): a malformed envelope, a bad signature, an authority object that does not fit the message |
| `unsupported` | `-32602` (invalid params): the receiver does not take this kind of message |
| `request_id_reused` | `-32602`: the `request_id` was already used for another message or recipient |
| `policy` | `-32603` with `reason: "policy"`: the team's rules deny the message |
| `declined` | `-32603` with `reason: "declined"`: the person answered no |
| `not_member` | `-32603` with `reason: "not_member"`: the sender is not a member of the receiver's team |
| `not_a_member` | `-32603` with `reason: "not_a_member"`: the recipient is not a member of the team |
| `loop` | `-32000`: the message would loop between agents |
| `no_worker` | `-32603` with `reason: "no_worker"`: no worker of that kind is offered |
| `busy` | `-32603` with `reason: "busy"`: the worker host is at its limit; try later |
| `cancelled` | task state `TASK_STATE_CANCELED`: the asker cancelled the assignment while it waited |

A message held for a person's answer is not an error. Its receipt state is `held`, which maps to
the task state `TASK_STATE_AUTH_REQUIRED` until the person allows or declines it (`declined` above).
