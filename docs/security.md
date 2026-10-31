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

An invite is the approval: whoever holds a live link joins with no one asked, and every member of
the network is trusted. In a flat network any member can mint a link and so add a member that is
not verified (shown "(not verified)"). Accepted: members are trusted. Removing a member
(`lightbulb remove`) is how you take that back. When an agent's invite brings someone in, a
dialog tells the person and offers Remove; an agent's invite is always one-time.

There is one kind of member. Every member is its own person: its person id is its own key, and
nothing in the roster can say otherwise (an admit's other claims are ignored). A Lightbulb
workspace's member (`lightbulb member`) is a member like any other; what it answers is its own
local rule. The Lightbulb backend writes it `members`: its people's Lightbulb user ids. It takes
a message for one of its agents only from a computer whose sign-in proof names one of them; any
other member's message (no proof, an expired one, another person's, another computer's) is
refused (`not_a_member`). `lightbulb login` proves the key: the backend sends a
one-time challenge (2 minutes), this computer signs `{challenge, key, purpose}` with its member
key, and the signed-in person types the code their terminal shows on the confirm page (10
minutes, single use). The answer is the sign-in proof: a compact EdDSA JWS from Lightbulb,
`{iat, exp, sub: <user id>, computer: <member key>}`, kept in `login.json` (0600) and shown in
this computer's `hello`. A workspace member verifies it offline against the issuer's public key.
Proofs last 30 days and are renewed in their last week (renewal asks the account server, which
can refuse); removing a person from a workspace takes effect at once (`members`), and a computer
whose key is in the sessions file's `revoked_keys` is refused even with a valid proof. `lightbulb logout` deletes
the proof. A
workspace member joins one network when a workspace admin runs
`lightbulb invite --agents`, which hands it a one-time link that admin's computer signed (the admin
confirmed it in the browser, so that computer admits it without a dialog). It also mints one-time
links, each carrying its doorbell under its signature, which Lightbulb gives only to a signed-in
person of that workspace: `lightbulb login` joins a computer that is alone on its own network with
one. Labels ending "(not verified)" are unproven names, never an
identity.

### Invite codes

`lightbulb invite` also prints a four-word code (EFF short wordlist, about 41 bits). The code
is a convenience; the full line or link is the strongest path. While the inviter's daemon
runs, a second endpoint whose key is derived from the code (Argon2id, 64 MiB, 3 passes, so
each guess is costly) serves the full invite once. It closes after the first hand-off, after 5
bad callers, after 10 minutes, or when the invite expires, and nothing listens when no code
invite is live. The code never serves the printed invite: it serves a fresh one-time invite
made for it alone, so a code works once. The
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

## Trust

Everyone on the network is trusted. A message from a member reaches the session it names; nothing
is held and there is no per-person Allow or policy. Membership is the only gate, and `lightbulb
remove <name>` ends it. (A workspace member also checks the sender's sign-in proof, see above.)
Messages between sessions on this computer always get through.

A join is admitted by its invite, with no dialog. The signed `admit` records who minted the
invite when it was an agent's, down to the session (`<member>/<session>`), shown as "admitted by"
in `lightbulb team`.

An agent may invite and join; only a person at a terminal can remove a member, leave a network or
change the relay: a command run from an agent's shell or tools is refused. It does not stop a
program that you run yourself as your own user. The one prompt left is an agent's permission
request that the Lightbulb app files (`lightbulb held`, `approve H<n>`).

## Memory

What your agents remember is stored in `state.db` as entries, each signed by the computer that
wrote it.

- **What syncs where.** `team` entries go to every member of the team. `me` entries go only to
  your own signed-in computers; without a sign-in a `me` entry stays on this computer.
  `lightbulb memory team off` stops this computer writing `team` entries.
- **Secrets are refused, not scrubbed.** `remember` rejects an entry that looks like an API key,
  a token, a private key, or a credential in a URL (`https://user:password@host`), and stores
  nothing. It does not try to remove the secret and keep the rest. The check is a pattern match,
  so it will miss some and flag some. Do not rely on it to catch everything. An entry from
  another computer that matches is refused when it arrives: this computer keeps a marker in its place, never the text.
- **Vectors stay here.** With the memory model installed, each entry and each of your prompts
  and replies gets a vector, made and kept on this computer only. Vectors are never sent to
  teammates or to Lightbulb, so computers on an older build keep syncing entries as before.
- **`forget`.** It deletes the entry's text on this computer, clears the memory blocks kept for
  `lightbulb memory last` (the next session start makes a new one), and sends the delete to every other
  computer, which applies it when it comes online. Any member's `forget` applies, not only the
  author's. A computer that never comes online again keeps what it held, and so does anyone who
  copied it before. The entry's id stays as a marker.
- **No block without an origin.** A session is handed the memory for its repository, found from
  the git origin. A folder with no git origin gets no block; its agent can still call `recall`.
- **Who can change it.** `memory forget`, `memory share` and `memory team` are refused when run from an agent's shell or tools, like `remove`; `list`, `show`, `last` and `export` are open. An agent still writes entries with `remember`.
- **The log.** `daemon.log` never holds an entry's text.

## Capture and upload

- **What is captured.** Each agent's hooks tell the background service when a session starts,
  after each tool call, at the end of each turn and when it ends. It keeps the session (its id,
  the agent, the computer, the repository and branch, when it started and ended, and token
  counts, for Claude Code only, read from its transcript once at the end) and for each tool
  call its number, time, tool, duration, whether it worked, and a summary of at most 200
  characters (a command, a path, a pattern). Never file contents, diffs or command output.
  It also records your prompts and the agent's replies, each cut to 4 KB, on your computer only;
  they are never uploaded, in any mode, and are deleted after 90 days (`capture_turns_days`).
  `lightbulb sessions turns off` stops it, from your own terminal only, and `recall` stops using
  the turns already stored; `lightbulb sessions forget <ID>` deletes them. A summary, prompt or
  reply that looks like a password or key is stored as `[refused: <what it matched>]` and is
  never given a vector. An agent's `recall` can show excerpts (up to 800 characters each) of your prompts and replies
  from the same repository ([usage](usage.md#memory)). Sessions under a folder hidden with `lightbulb folder <path> off`, or under a dot
  folder in your home folder (`~/.config`), are not captured.
- **Where it lives.** In `state.db` on this computer. Teammates never receive it.
- **What is uploaded.** Nothing unless you choose. `lightbulb sessions upload off` (the default)
  sends nothing and makes no request. `metadata` sends each finished session's agent,
  repository, branch, times and token counts, and for each tool call its number, time, tool,
  duration and whether it worked, without the summary. `full` adds the summary. Uploads go only
  while this computer is signed in, only for sessions that finish after you turn them on, and
  are signed with this computer's identity. Only you can change the mode: `sessions upload
  <mode>` is refused when run from an agent's shell or tools.
- **The evaluator.** It reads only sessions you uploaded in `full`, when they arrive, and
  suggests lessons. A suggestion does nothing until you keep it with `lightbulb memory approve`;
  `lightbulb memory dismiss` drops it. `metadata` uploads are never read by it.
- **Deleting.** `lightbulb sessions forget <id>` deletes a session, its tool calls and its prompts and replies on this
  computer; it does not delete a copy already uploaded. `lightbulb sessions upload off` stops
  further uploads and forgets where the last one stopped.

## Loop limits

Two limits stop agents from messaging each other forever:

- **Rate:** at most 12 messages in 10 minutes from one sender to one session. Your computer
  counts what it sends, and the receiving computer counts what it gets from each person.
- **Hops:** each agent reply in a chain counts one hop. A message a person types is hop 0. A
  message at hop 20 is refused, so 19 agent replies get through. A person must speak to start a
  new chain.

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
| `state.db` | SQLite: the roster, sessions, messages, receipts, memory entries, the memory block each session was last handed (`lightbulb memory last`), captured sessions, their tool calls and your prompts and replies (see "Capture and upload"), the vectors for memory search, permission prompts and their answers. A message's text is deleted once nothing waits on it (read, refused or expired); messages to your own terminal keep theirs. |
| `model/` | The memory model, if setup downloaded it. |
| `upload.mark` | While uploads are on: the time up to which finished sessions were sent. Deleted by `lightbulb sessions upload off`. |
| `daemon.log` | The log (see above). |
| `addrs.json` | Teammates' last known network addresses. |
| `sock`, `daemon.lock` | The service's socket and lock. |
| `bin/lightbulb` | The program. |

To see your Claude Code sessions, the background service runs `claude agents --json --all` periodically; it only reads.

How long things last:

| What | How long |
|---|---|
| A message waiting for an offline computer | retried for 24 hours, then `expired` |
| A message waiting on the receiving computer for its session, not picked up | 24 hours, then `expired` |
| A permission prompt with no answer | 10 minutes (sooner if its session has ended) |
| A one-time invite / a reusable invite | 24 hours / 7 days |
| Answered join requests | kept 30 days |
| A prompt or reply | 90 days (`capture_turns_days`) |

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
(not the full path), the repository and branch, whether it is busy, waiting for its person (and what for) or idle, its title, and what
it is doing now (from the agent's to-do list). For seven days after a session ends, they also see
its name, folder, repository, branch, when it ended and why; the command that resumes it stays on your computer. They never see your files or your agents'
conversations, only the messages sent to them. Hide a folder with `lightbulb folder <path> off`.

## Telemetry and network requests

Lightbulb sends anonymous usage counts (see "Usage counts" in the README; off with `lightbulb telemetry off`, or `DO_NOT_TRACK=1` / `LIGHTBULB_NO_TELEMETRY=1`, which always win). It sends no crash reports and no message content. The first `setup` sends one `cli_installed` event before it prints the opt-out notice; set one of those variables before installing to send nothing at all.

Apart from those counts, the engine calls Lightbulb only for these: `lightbulb login` (a device
sign-in you confirm in the browser), renewing a signed-in computer's proof, `lightbulb invite
--agents`, while this computer is signed in one live connection that hosted apps reach your
sessions through, `POST /sessions` (the uploader: every five minutes while signed in and
uploads are `metadata` or `full`, and only when a session has finished since the last one; never
while uploads are `off`), and `GET /memory/drafts` with `POST /memory/drafts/<id>/approve` or
`/dismiss` (only when you run `lightbulb memory drafts`, `approve` or `dismiss`). It fetches no
relay list from Lightbulb. Otherwise it talks only to your
teammates' computers and, when no direct path exists, to the relays (the iroh project's public
relays unless you or your team set one).

`lightbulb setup` downloads the memory model once from Hugging Face (`huggingface.co`, one pinned
revision, each file checked by SHA-256); `setup --no-model` skips it.

The install script, `npm install`, `lightbulb update` and the daemon's daily update check
download the release from GitHub. An update checks the tarball's SHA-256 against the `.sha256`
file of the same GitHub release: that proves the download is intact, with the same trust as
the install script (whoever controls the release controls both). On macOS an update also
requires the new binary to be signed with Lightbulb's Developer ID (team `WZX4U3C22Y`) and
notarized. On Linux, `lightbulb update` also checks the checksum file's Ed25519 signature
(`.sha256.sig`) against a key built into the binary. The install script checks the SHA-256 only. The background daily update never edits your shell
profile. Only a command you run at a terminal (`setup`, or `update` there) adds `~/.local/bin`
to your PATH, once, and never through a symlinked profile.

## Reporting a vulnerability

Email `hello@thelightbulb.company` with the subject "security". See [SECURITY.md](../SECURITY.md).
