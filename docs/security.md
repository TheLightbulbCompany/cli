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

The team's membership is a log of entries: who was admitted, removed, made an admin or the
founder, who left, which invites exist, and the team's relay and name. Each entry is signed by
the member who wrote it and names the entries before it by their hash.

Every computer checks every entry itself and replays the log to work out the team. An entry
counts only if its signer had the right to write it: for example, only the founder or an admin
can remove a member, and only the founder can make an admin. A new member checks every entry it
receives before it accepts the team. No server holds or decides the membership.

Invites are signed by the computer that made them. A join is checked against the roster: the
invite must still be live (not replaced, not ended by a removal, not a used one-time invite) and
not expired.

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
- Your other computers count as you only after both sign in. Sign-in (verified names) is not
  available in this release, so each computer counts as its own person, and your other
  computer's first message is held once, like anyone's.
- A person with no policy entry is held.
- Allow lets that person's later messages through (`policy <person> accept`).
- `policy <person> hold` asks each time; `policy <person> refuse` refuses them.

A join request is held the same way, unless the founder or an admin made the invite with
`invite --open`.

Only a person at a terminal can answer a hold, change membership or change the relay. A command
run from an agent's shell or tools is refused. This stops an agent's ordinary tools from
approving things for you. It does not stop a program that you run yourself as your own user.

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
| `state.db` | SQLite: the roster, sessions, messages, receipts, holds and their answers, policy. |
| `daemon.log` | The log (see above). |
| `addrs.json` | Teammates' last known network addresses. |
| `relays.json`, `relays.json.minisig`, `relays.floor` | The signed relay list, its signature, and the newest list date seen (only while no relay is set). |
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

Message text is kept in `state.db` after delivery; nothing deletes it. `leave` and
`setup --remove` keep it too. To erase this computer's key and messages, run
`lightbulb setup --remove`, then delete `~/.lightbulb/connect/` and the command link
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

Telemetry: none. Lightbulb sends no usage data, no crash reports and no analytics.

It talks to your teammates' computers and to relays. Apart from that, it makes these requests to
the Lightbulb server (`https://thelightbulb.company/connect`):

- **The signed relay list:** while neither you nor your team has set a relay, the service asks
  for `relays.json` and its signature at most once a day (once an hour after a failure). It sends
  no data about you; the server sees your IP address. Setting a relay (`lightbulb relay <url>`)
  stops it. A list is used only if it carries a valid signature.
- **Sign-in:** `lightbulb signin`, when you run it, and then renewing that sign-in about once a
  day.

The install script and `npm install` download the release from GitHub.

## Reporting a vulnerability

Email `hello@thelightbulb.company` with the subject "security". See [SECURITY.md](../SECURITY.md).
