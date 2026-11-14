# The Lightbulb wire: envelopes and refusals

A message travels between two computers of one team over an authenticated QUIC connection (the
peer's key is its member key, and only roster members are served), as one JSON frame
(`envelope`) of at most 1 MiB. The receiving computer answers with an `ack`, and later with a
`receipt` when the message's state moves on.

## Envelope

| Field | Meaning |
| --- | --- |
| `id` | the envelope's ULID; a resend with a seen `id` gets the state it has now |
| `request_id` | the sender's id for the message; reused with another text or recipient it is refused |
| `kind` | `message`, `assign`, `result` or `cancel` |
| `from`, `to` | session addresses; `from` must be under the sending member's prefix |
| `reply_to`, `thread` | the message this answers, and its thread |
| `hops` | agent replies deep; bounded to stop loops |
| `body` | the text |
| `meta` | an assignment's or result's fields |
| `sig` | an Ed25519 signature by the sending computer over the envelope as sent, without `sig` |

An envelope with an `a2a` field (an older build's A2A Message) is refused `unsupported`.

## Refusals

A refusal reaches the sender with state `refused` and one of these reasons:

| Reason | Why |
| --- | --- |
| `bad_request` | a malformed envelope or a bad signature |
| `unsupported` | the receiver does not take this kind of message |
| `request_id_reused` | the `request_id` was already used for another message or recipient |
| `policy` | the message may not go to that address |
| `declined` | the person answered no |
| `not_member` | the sender or the recipient is not a member of the receiver's team |
| `not_a_member` | sent by older builds only |
| `loop` | the message would loop between agents |
| `no_worker` | no worker of that kind is offered |
| `busy` | the worker host is at its limit; try later |
| `cancelled` | the asker cancelled the assignment while it waited |

A worker start held for a person's answer is not an error: its state is `held` until the person
allows or declines it, and `expired` if nobody answers in time.
