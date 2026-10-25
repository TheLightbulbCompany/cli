# Security

This page says what protects your messages, what other people and servers can see, and what
Lightbulb keeps on your computer. To report a vulnerability, see [SECURITY.md](../SECURITY.md).

## Identity keys

Each computer has one Ed25519 key pair. The background service makes it the first time it
starts and keeps the secret half in `~/.lightbulb/connect/identity.key` (32 bytes, mode `0600`).
The key never leaves the computer.

The public half is the computer's **member key** (lowercase base32). Its first 12 characters
are the computer's short address, which you see in addresses such as `<short>/codex:<id>`.
The same key is the computer's network identity: other computers dial it by this key.

## The signed roster

The team's membership is a log of entries: who was admitted (and by whom), removed, who left, which invites exist, and the team's relay and name. Each entry is signed by
the member who wrote it and names the entries before it by their hash.

Every computer checks every entry itself and replays the log to work out the team. An entry
counts only if its signer had the right to write it: every network is flat, so any computer
member can invite, admit and remove others (not itself, and never the founder, which may only
leave). A new member checks every entry it
receives before it accepts the team. No server holds or decides the membership.

Invites are signed by the computer that made them. A join is checked against the roster: the
invite must still be live (not replaced, not ended by a removal, not a used one-time invite) and
not expired.

In a flat network any member can mint a one-time link and so add a member that is not
verified (shown "(not verified)"). Accepted: members are trusted.

There is one kind of member. Every member is its own person: its person id is its own key, and
nothing in the roster can say otherwise (an admit's other claims are ignored). A Lightbulb
workspace's member (`lightbulb member`) is a member like any other; what it answers is its own
local rule. It keeps, in its own state alone, which computer belongs to which workspace person:
it records that when a computer joins through a `person` link the workspace itself made for that
person (the link names the person under the workspace's signature and works once). With a
members list, it takes a message for one of its agents only from a computer it recorded for a
listed person; any other member's message is refused (`not_a_member`), with no hold. No frame a
peer sends writes that record, and it is dropped by any install whose replay applies a removal
or leave of the computer (even if it is admitted again in the same sync); an entry replay
ignores (a leave signed for someone else, a removal of the founder) drops nothing. A computer
that joins with a person line accepts from its inviter only an answer to its own message: its
`reply_to` must name a message, at most a day old, that the session it is sent to sent to the
very session it comes from. Anything else from the inviter, a continued thread included, is held
like anyone's, and the mark goes when the inviter leaves the team. Labels
ending "(not verified)" are unproven names, never an identity. Only join with a person line from a workspace you
trust; an ordinary invite line or code never does this.

### Invite codes

`lightbulb invite` also prints a four-word code (EFF short wordlist, about 41 bits). The code
is a convenience; the full line or link is the strongest path. While the inviter's daemon
runs, a second endpoint whose key is derived from the code (Argon2id, 64 MiB, 3 passes, so
each guess is costly) serves the full invite once. It closes after the first hand-off, after 5
bad callers, after 10 minutes, or when the invite expires, and nothing listens when no code
invite is live. The code never serves the printed invite: it serves a fresh one-time invite
made for it alone, which asks the inviter, so a join by code always needs the inviter's Allow,
even when the printed link is open or reusable. The
code is never logged, but it appears in shell history and `ps` on the joiner's computer; it
is dead once used or after 10 minutes. 5 junk connections can close a live code (denial of
service only; fall back to the full line).

The key is also salted with the current 10-minute window, so no one can build a table of
every code's key ahead of time: a guess only counts for the window it was made in. What is
left: someone who sees the code endpoint on a relay and can run about 2^41 Argon2id guesses
inside one 10-minute window could pose as the inviter and hand the joiner a different
network's invite. That takes on the order of a billion cores at once, so we accept it for
now; a password-authenticated key exchange (as in magic-wormhole) would close it, and the
full line never had it.

## Transport encryption

Computers talk over QUIC, using the iroh library. Each connection is encrypted end to end
between the two computers' keys, and each side proves its key. A computer drops a connection
from a key that is not a member, except a request to join, which only asks. Nothing is sent in
plain text.

### Relays

When two computers have no direct path, iroh carries their encrypted connection through a relay
server. By default these are the iroh project's public relays; a team can use its own (see
[teams](teams.md#relays)).

A relay **can** see: the member keys of the two computers, their IP addresses, and when and how
much they send.
A relay **cannot** see: message text, session lists, names, or anything else inside the
connection. It cannot change what is sent without the change being detected.

### On your local network

With the `lan` setting on (the default), the service announces its member key and port on your
local network over mDNS, so your other Macs on the same Wi-Fi find it. Anyone on that network can
see the announcement. To turn it off, add `lan = false` to `~/.lightbulb/connect/config.toml`.
Joins still work, because the invite carries the inviter's addresses.

## Holds and policy

A message from someone you have not allowed does not reach your sessions. It is held until you
answer it (Allow or Decline in a dialog, or `lightbulb approve H<n>` / `decline H<n>`).

- Messages between sessions on this computer always get through.
- Verified names are not available yet, so each computer counts as its own person, and your
  other computer's first message is held once, like anyone's.
- A person with no policy entry is held.
- Allow lets that person's later messages through (`policy <person> accept`).
- `policy <person> hold` asks each time; `policy <person> refuse` refuses them.

A join request is held the same way, unless the invite was made with `invite --open`. An approval records who gave it, down to the
session (`approved by <member>/<session>`), shown on receipts and as "admitted by" in `lightbulb team`.

An agent may invite, join, approve a join and answer a hold; its answer is recorded under its
session, and its skill tells it to ask you first. Only a person at a terminal can remove a member,
leave a network or change the relay: a command run from an agent's shell or tools is refused. It does not stop a program that you run yourself as your own user.

## Loop limits

Two limits stop agents from messaging each other forever:

- **Rate:** at most 12 messages in 10 minutes from one sender to one session. Your computer
  counts what it sends, and the receiving computer counts what it gets from each person.
- **Hops:** each agent reply in a chain counts one hop. A message a person types is hop 0. A
  message at hop 4 is refused. A person must speak to start a new chain.

A refused message's receipt says which limit it hit. A message is at most 64 KiB of text.

## What is logged

The service writes `~/.lightbulb/connect/daemon.log`. It logs events: connections, member key
prefixes, message and session ids, receipt states, hold and join request numbers, relay URLs.

It never logs message text, message previews or file contents. When your computer is removed
from a team, it logs the team's name and the name of whoever removed it.

## Data on your computer

Everything lives in `~/.lightbulb/connect/`, which is private to your user (`0700`; its socket
is `0600`):

| File | What it holds |
|---|---|
| `identity.key` | This computer's secret key. |
| `config.toml` | Settings (computer name, relay, `lan`, hidden folders). |
| `state.db` | SQLite: the roster, sessions, messages, receipts, holds and their answers, policy. A message's text is deleted once nothing waits on it (read, refused or expired); messages to your own terminal keep theirs. |
| `daemon.log` | The log (see above). |
| `addrs.json` | Teammates' last known network addresses. |
| `sock`, `daemon.lock` | The service's socket and lock. |
| `bin/lightbulb` | The program. |

How long things last:

| What | How long |
|---|---|
| A message waiting for an offline computer | retried for 24 hours, then `expired` |
| A message waiting on the receiving computer for its session, not picked up | 24 hours, then `expired` |
| A held message with no answer | 7 days (sooner if its session has been gone for 24 hours) |
| A one-time invite / a reusable or open invite | 24 hours / 7 days |
| Answered join requests | kept 30 days |

A message's text is deleted from `state.db` once nothing waits on it (read, refused or
expired); its row stays for receipts, loop limits and request-id replay. Messages to your own
terminal keep their text. `leave` and `setup --remove` keep what is left. To erase this
computer's key and messages, run `lightbulb setup --remove`, then delete `~/.lightbulb/connect/` and the command link
`~/.local/bin/lightbulb`:

```sh
rm -rf ~/.lightbulb/connect ~/.local/bin/lightbulb
```

## What teammates see

For each of your sessions that is not hidden: the agent, the computer, the folder's last part
(not the full path), the repository and branch, whether it is busy or idle, its title, and what
it is doing now (from the agent's to-do list). They never see your files or your agents'
conversations, only the messages sent to them. Hide a folder with `lightbulb folder <path> off`.

## Telemetry and network requests

Lightbulb sends anonymous usage counts (see "Usage counts" in the README; off with `lightbulb telemetry off`, or `DO_NOT_TRACK=1` / `LIGHTBULB_NO_TELEMETRY=1`, which always win). It sends no crash reports and no message content. The first `setup` sends one `cli_installed` event before it prints the opt-out notice; set one of those variables before installing to send nothing at all.

Apart from those counts, the engine makes no network call to Lightbulb: no relay list, no sign-in. It talks
only to your teammates' computers and, when no direct path exists, to the relays (the iroh
project's public relays unless you or your team set one).

The install script, `npm install`, `lightbulb update` and the daemon's daily update check
download the release from GitHub. An update checks the tarball's SHA-256 against the `.sha256`
file of the same GitHub release: that proves the download is intact, with the same trust as
the install script (whoever controls the release controls both). On macOS an update also
requires the new binary to be signed with Lightbulb's Developer ID (team `WZX4U3C22Y`); on
Linux the checksum is the only check. The background daily update never edits your shell
profile. Only a command you run at a terminal (`setup`, or `update` there) adds `~/.local/bin`
to your PATH, once, and never through a symlinked profile.

## Reporting a vulnerability

Email `hello@thelightbulb.company` with the subject "security". See [SECURITY.md](../SECURITY.md).
