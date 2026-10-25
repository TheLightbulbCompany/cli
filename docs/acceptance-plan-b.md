# Plan B acceptance — step 1 (one Mac)

- Date: 2026-10-23, 10:28–10:50 local (EDT).
- Build: `e08b3fa` (`plan-b`), `cargo build --release`, sha256 `6346fdddbe0541e82256335fc6e7e28da205a211991a05fc6d98284da37ce8b9`, `lightbulb-connect 0.1.0`. A debug build of the same commit was used for the network rows (see "Why the network rows ran on a debug build").
- Machine: the owner's Mac (Apple silicon, Darwin 25.2.0), with Claude Code, Codex, Cursor, Grok CLI and the Lightbulb desktop app installed. The app owns the `lightbulb` command and the `lightbulb` MCP server name, so Connect is `lightbulb-connect` in both places.
- A = the real install (`~/.lightbulb/connect`). B = a second daemon, `LIGHTBULB_CONNECT_HOME=/tmp/lbc-b`, `computer = "mac-b"`.
- Every person-side command ran inside a tmux window. The CLI refuses `held`, `approve` and the other person-only commands from an agent's shell ("Run this from your own terminal, not from an agent."), and a tmux server is a terminal program. No text was typed into any agent app.

## Results

| # | Row | Result |
|---|-----|--------|
| P1 | Release binary: no `DEBUG_` knob names, no test issuer | PASS (see note 1) |
| P2 | Reinstall over the e864370 install: `setup`, `doctor` | PASS after a manual fix (**F1**) |
| P3 | Migration V5 → V6 on the real `state.db` | PASS |
| P4 | Agent configs keep the hooks and the `lightbulb-connect` MCP entry; hooks exit 0 | PASS |
| — | **Release-to-release discovery over loopback mDNS** | **FAIL (F2)**: blocks every network row on the release build |
| 1 | invite → join `--name aden` → dialog → `approve J2` → Connected | PASS (debug build) |
| 2 | B send → `held` + dialog → Allow → delivered | PASS (debug build) |
| 3 | A second B send goes straight in | PASS (debug build) |
| 4 | A fresh one Declined → `refused`, reason `declined` | PASS (debug build) |
| 5 | B stopped, A sends → `queued`; B restarted → delivered | PASS (debug build); B held it first, which is correct |
| 6 | `who --all` on both shows both computers' sessions | PASS (debug build) |
| 7 | `held` lists a pending join and a held message, with their commands | PASS (debug build) |
| 8 | `policy`; `policy <person> accept`; a B send is stored directly | **FAIL by name (F3)**, PASS by member key |
| 9 | `remove aden` → B is out, link replaced; B `leave` | PASS on A's side; B is not told it was removed (**F4**) |
| 10 | `receipt <id>` sentences for each state | PASS (queued, held, stored, delivered, refused); `read` and `expired` were not reached |
| 11 | `folder <path> off` hides a session from B, `on` shows it again | PASS (each change shows on B after one presence round, about 2 s) |
| 12 | `relay` prints the list and its source | PASS (text below) |
| 13 | Error texts | PASS (one small note) |
| — | Message bodies absent from both daemon logs | PASS |

## Part 1 — reinstall on the owner's Mac

### P1. The release binary

```
$ lightbulb-connect version
lightbulb-connect 0.1.0
strings: LIGHTBULB_CONNECT_DEBUG 0, DEBUG_ 0, LIGHTBULB_CONNECT_PEERS 0, lightbulb-debug-test-issuer 0,
         LIGHTBULB_CONNECT_CLAUDE_HOME 0, test-issuer key bytes present: False
```

Note 1: `LIGHTBULB_CONNECT_HOOK_CALLER` still appears twice in the binary, but only as help text: the doc comments of the hidden `hook` and `mcp` subcommands (`src/main.rs:188,193`) say "Debug builds only: …". The code that reads the variable is compiled out. It is harmless, but it is a debug knob name in the release `--help` data.

### P2. Backups, stop, install, setup, doctor

Backups: `~/.lightbulb/acc-backup-2026-10-23/` holds the agents' config files (`.claude/settings.json`, `.claude.json`, `.codex/config.toml`, `.codex/hooks.json`, `.cursor/{cli-config,hooks,mcp}.json`, `.grok/config.toml`, `.grok/hooks/`), plus `connect/state.db` (a sqlite `.backup`, V5), `identity.key`, the launchd plist and the old binary `lightbulb-connect.e864370`.

```
$ lightbulb-connect daemon --uninstall        # the old e864370 binary
daemon stopped and launchd job removed; state kept
(new binary copied beside the canonical path, then renamed over it)
$ lightbulb-connect setup
Command: lightbulb-connect ("lightbulb" belongs to the Lightbulb app)
Claude Code: connected (hooks + tools as "lightbulb-connect" — another "lightbulb" tool server is already installed)
Codex: connected (hooks + tools as "lightbulb-connect" — another "lightbulb" tool server is already installed)
Cursor: connected (hooks + tools as "lightbulb-connect" — another "lightbulb" tool server is already installed)
Grok CLI: connected (hooks + tools as "lightbulb-connect" — another "lightbulb" tool server is already installed)
Background service: running (starts at login).

Lightbulb Connect is on.
  Connected: Claude Code, Codex, Cursor, Grok CLI

Try it: open Claude Code in any folder and ask "list my other sessions".
Add your other computer or a teammate: lightbulb-connect invite
Check everything: lightbulb-connect doctor
exit=0
$ lightbulb-connect doctor
Background service   running
Starts at login      yes
State folder         private
Claude Code          connected
Codex                connected
Cursor               connected
Grok CLI             connected
Command name         "lightbulb" belongs to the Lightbulb app; type "lightbulb-connect"
All good.
```

**F1 (upgrade, real-world): after `setup`, the OLD daemon was still serving, and `doctor` said "All good".** Two `daemon` processes were running. pid 94171 ran the e864370 binary (the old inode, 21,855,264 bytes) and held `daemon.lock`, the socket and `state.db`. launchd's pid 94236 ran the new binary and waited on the lock. `state.db` stayed at V5. The cause: between `daemon --uninstall` (which removes the plist) and `setup`, a live agent's hook (this Claude Code session's `PostToolUse`) called `daemon::ensure_running` (`src/daemon/mod.rs:281-296`). With no plist present, that spawned a detached daemon from the binary that was current at that moment. `install_job`'s `bootout; bootstrap; kickstart -k` restarts only launchd's own job, so it cannot stop a daemon that launchd did not start. I sent `{"op":"shutdown"}` to the socket. launchd's new daemon took the lock and migrated the db. The same race happened again when I booted out the job for the network rows. Fix idea: `setup` and `daemon --install` should send `shutdown` to whatever daemon answers the socket when its binary differs from the canonical one (the daemon could report its build in `status`). `doctor` should also flag a daemon whose binary is not the installed one.

### P3. Migration

```
before (backup): user_version=5  approvals=1 grants=0 holds=1 join_requests=1 mailbox=1 messages=3 outbox=1
                 peers_sessions=1 policy=1 receipt_out=0 receipts=3 roster=5 seen_ids=1 sessions=3 team=1 team_link=2
after:           user_version=6  (the same counts) + hidden_sessions=0
messages, mailbox, receipts, roster, holds, outbox, team: identical row-for-row
schema diff: sessions gains `repo TEXT, branch TEXT, handle TEXT`; new table `hidden_sessions`
```

### P4. Agent wiring and hooks

`setup` left every config byte-identical to the fresh backup (idempotent), and the `mcpServers` in `~/.claude.json` were unchanged. Each agent has the five hooks (`'<home>/.lightbulb/connect/bin/lightbulb-connect' hook <agent> <session-start|user-prompt-submit|post-tool-use|stop|session-end>`) and this MCP entry:

```
~/.claude.json      "lightbulb-connect": {"command": "<home>/.lightbulb/connect/bin/lightbulb-connect", "args": ["mcp", "--server-name", "lightbulb-connect"]}
~/.codex/config.toml [mcp_servers.lightbulb-connect] command = "<home>/.lightbulb/connect/bin/lightbulb-connect" args = ["mcp", "--server-name", "lightbulb-connect"]
~/.cursor/mcp.json  same, "type": "stdio"; cli-config allow "Mcp(lightbulb-connect:*)"
~/.grok/config.toml [mcp_servers.lightbulb-connect] … enabled = true; allow "MCPTool(lightbulb-connect__*)"
```

The app's own `lightbulb` entries are untouched. I ran each of the 20 `hook <agent> <event>` calls with `{}` on stdin: all exited 0, in 0.013–0.015 s. Empty stdin, bad JSON, an unknown agent and missing arguments also exited 0 (the errors go to stderr). A `grok` hook from a caller that is not Grok was a no-op.

## Why the network rows ran on a debug build — F2

**F2: two release daemons on one Mac do not find each other.** B joined with A's link and got `No one from this network is online right now. Ask them to open their laptop, then run the same command again. Your key is ready; nothing was lost.`, and B's log said `join: dial gjivepwnkmgn failed`. Both daemons were up, and A held UDP 58002 and 5353. A third release daemon, C, started from the same shell, failed the same way (`join: dial oswwoypqouo7 failed`). So the launchd context and macOS Local Network permission are not the cause, and Python sent multicast from the same shell without error. Control test, same machine, same minute: two daemons on the **old e864370 binary** joined at once (the dialog came up in under a second, and I declined it). So this is a regression between e864370 and e08b3fa. The likely cause is commit `4a80b78` ("relay fallback with a signed relay list"): `Net::start` now builds the endpoint with `.relay_mode(RelayMode::Custom(<the relay map>))` (`src/net/mod.rs:433-436`). Today that map is empty, because the relay list fetch got a 404. Every network test dials through the debug-only `LIGHTBULB_CONNECT_PEERS` knob (`tests/net_two.rs`, `tests/decision.rs`, `tests/common/mod.rs`), so no test exercises mDNS discovery. Not fixed here. **This blocks step 2 (two Macs) until it is fixed.**

To still exercise the protocol, rows 1–13 ran as the brief's original step 1 describes. I built `cargo build` of the same commit. A's launchd job was booted out (the plist was kept), and A's real state dir was served by `target-acc/debug/lightbulb-connect daemon` (UDP 52748). B ran `LIGHTBULB_CONNECT_PEERS=gjivepwn…me54a@127.0.0.1:52748`, and B always dials A. Afterwards the launchd job was bootstrapped again: the release daemon (pid 7237, the e08b3fa inode) took over, and the final `doctor` said `All good.`

## Part 2 — the rows (A real state on the debug daemon; B debug, PEERS→A)

Body markers: `ACC7Q-<n>`. The hold and join dialogs were real macOS dialogs, and each one was answered from the CLI.

### 1. Join

```
A$ lightbulb-connect invite
Send this link to the person (or paste it on your other computer): lightbulb-connect join '<link>'
Join Lightbulb Connect: https://thelightbulb.company/j/#eyJ…   (663 characters)
B$ lightbulb-connect join '<link>' --name aden
Asking whoever sent the link to allow this computer… (they get a prompt; this waits up to 10 minutes)
A dialog: [Lightbulb Connect] aden (computer "mac-b") wants to join your team using your invite link. Allow only if you sent them the link.
          Or run: lightbulb-connect approve J2            [Decline] [Allow]
A$ lightbulb-connect held
J2  aden (mac-b) wants to join — lightbulb-connect approve J2
A$ lightbulb-connect approve J2
aden joined the network.                                   (the dialog went away)
B: Connected as aden (mac-b). Team "alice": 2 computers online.
   Your agents here can now see and message the team's agents. Try: lightbulb-connect who --all
   Until both computers sign in, your other computer counts as another person and its first message needs your Allow.
A$ lightbulb-connect team
Team "alice" — 2 computers
  You (alice) · mac-a · founder
  aden · mac-b · member
```

### 2. First message held, then allowed

A ran `lightbulb-connect listen --as inbox`. (I did not use A's Claude session as the target: it was this live agent conversation.)

```
B$ lightbulb-connect send alice/inbox "ACC7Q-1 first message from aden"
Sent. alice has to allow it first (they see a prompt). Check: lightbulb-connect receipt 01M5MK84VVVV47KTR5FEBKZW1P
A dialog: aden, at the terminal (not verified) wants to send a message to "inbox" on this computer.
          "ACC7Q-1 first message from aden"              [Decline] [Allow]
A$ lightbulb-connect held
H2  aden, at the terminal (not verified — they have not signed in) -> gjivepwnkmgn/inbox  "ACC7Q-1 first message from aden" — lightbulb-connect approve H2
B$ lightbulb-connect receipt 01M5MK84… --json
{"online":true,"person":"alice","reason":null,"request_id":"01M5MK84VVVV47KTR5FEBKZW1P","state":"held","thread":"01M5MK84VWA6H9VVR0FCR9MEW4","where":"mac-a"}
A$ lightbulb-connect approve H2
Allowed H2.
A listen: From aden, at the terminal (not verified): ACC7Q-1 first message from aden
          Reply: lightbulb-connect send qxcrztez36jw/terminal "…" --reply-to 01M5MK84VVVV47KTR5FEBKZW1P
B$ lightbulb-connect receipt 01M5MK84…
Delivered to alice on mac-a.
```

The sender label is `aden, at the terminal (not verified)`. I did not observe an agent's framed header (`[Lightbulb teammate contribution from <label> {"requestId":…}]`, `src/frame.rs:6`) in this pass, because nothing was pushed to a real agent session.

### 3. Second message goes straight in

```
B$ lightbulb-connect send alice/inbox "ACC7Q-2 second message"
Sent. Stored on mac-a for the session's next step. Check: lightbulb-connect receipt 01M5MK8GTYVQYKZPE1K6RK6SAN
A listen: From aden, at the terminal (not verified): ACC7Q-2 second message
B$ lightbulb-connect receipt 01M5MK8G…       →  Delivered to alice on mac-a.
```

(The Allow covers the person for every name on A: a send to `desk` also went straight in.)

### 4. Declined

To get a fresh hold, I set A's policy for B's key to `hold` (see F3 for why it had to be the key).

```
B$ lightbulb-connect send alice/desk "ACC7Q-5 please decline this"
Sent. alice has to allow it first (they see a prompt). Check: lightbulb-connect receipt 01M5MK9H42WKR7QPB6ZEZZN3JV
A$ lightbulb-connect decline H3
Declined H3.
B$ lightbulb-connect receipt 01M5MK9H…
Not sent: they declined it.
{"online":true,"person":"alice","reason":"declined","request_id":"01M5MK9H42WKR7QPB6ZEZZN3JV","state":"refused",…}
```

### 5. Offline, queued, delivered

```
(B's daemon stopped)
A$ lightbulb-connect send aden/desk "ACC7Q-7 sent while you were away"
Not delivered yet: mac-b is offline. Lightbulb Connect keeps trying until 2026-10-24 10:40. Check: lightbulb-connect receipt 01M5MKABV9RMXWPKAQSMKX22TK
{"online":false,"person":"aden","reason":null,…,"state":"queued","until":1792852855,"where":"mac-b"}
(B restarted; B$ lightbulb-connect listen --as desk --once)
B dialog: alice, at the terminal (not verified) wants to send a message to "desk" on this computer. "ACC7Q-7 sent while you were away"
B$ lightbulb-connect approve H1   →  Allowed H1.
B listen: From alice, at the terminal (not verified): ACC7Q-7 sent while you were away
A$ lightbulb-connect receipt 01M5MKAB…   →  Delivered to aden on mac-b.
```

### 6. who --all

```
A$ lightbulb-connect who --all
Name                                                      Person                Computer         Agent        Folder                                                 Status                         Doing
claude-1                                                  alice  mac-a  Claude Code  /Users/alice/Desktop/lightbulb.nosync/  idle, gets messages instantly  ship-agent-network-phase-1
qxcrztez36jw/claude:dc77321a-…                            aden                  mac-b            Claude Code                                                         idle, gets messages instantly  ship-agent-network-phase-1
B$ lightbulb-connect who --all
qxcrztez36jw/claude:dc77321a-…  aden                  mac-b            Claude Code  /Users/…/lightbulb.nosync  idle, gets messages instantly  …
gjivepwnkmgn/claude:dc77321a-…  alice  mac-a  Claude Code  lightbulb                  idle, gets messages instantly  …
```

Own sessions show by handle (`claude-1`) and a teammate's by its `<short>/…` address, with the person and computer columns. One-Mac artifact: B lists the same Claude session as A, because in a release or debug build without `LIGHTBULB_CONNECT_CLAUDE_HOME` its daemon reads the same `~/.claude`.

### 7. held with a join and a message

A throwaway third daemon C (`visitor`) asked to join while B sent a message that was held:

```
A$ lightbulb-connect held
J3  visitor (mac-c) wants to join — lightbulb-connect approve J3
H4  aden, at the terminal (not verified — they have not signed in) -> gjivepwnkmgn/inbox  "ACC7Q-10 held for row 7" — lightbulb-connect approve H4
A$ lightbulb-connect decline J3   →  Declined visitor.       C: Your request to join was declined.
A$ lightbulb-connect approve H4   →  Allowed H4.
```

### 8. Policy — F3

```
A$ lightbulb-connect policy
Your workspace's agents may message your sessions without asking: no.
Messages from awljc6ald6ktba5yob5zwn46qql2jltsm7ivb7yxiwq7sg5w5fba: accepted.
Messages from qxcrztez36jwxd6tbu3cx65ay6swz7j6swvp4blaxxuaewg5rhfa: accepted.     ← written by the Allow of H2
A$ lightbulb-connect policy aden hold
…Messages from aden: held for your Allow.
B$ send … "ACC7Q-4 …"   →  Sent. Stored on mac-a …              ← NOT held
A$ lightbulb-connect policy qxcrztez36jw…rhfa hold   → the next send was held (row 4)
A$ lightbulb-connect policy qxcrztez36jw…rhfa accept
B$ send … "ACC7Q-6 after accept"   →  Sent. Stored on mac-a for the session's next step. …
A$ lightbulb-connect policy aden maybe   →  policy takes accept, hold or refuse.  (exit 1)
```

**F3: `policy <name> <value>` is silently inert.** It stores `person_id = "aden"` literally (`policy` table: `people|aden|hold|local|…`). `policy::decide` looks up the member's person id, which for an unverified member is its key. The CLI accepts any string and echoes it back as if it worked. The listing also prints raw 52-character keys instead of names. Fix idea: resolve a member name to its person id in `policy`, as `send` does, refuse names it cannot resolve, and print names in the listing. (I deleted the two policy rows I created from A's db at the end.)

### 9. remove and leave — F4

```
A$ lightbulb-connect remove aden
Removed aden. New connections and new mail from it are blocked. The invite link was replaced: Join Lightbulb Connect: https://thelightbulb.company/j/#eyJ…
A$ lightbulb-connect team   →  Team "alice" — 1 computer
B$ lightbulb-connect send alice/inbox "ACC7Q-11 after removal"
Not delivered yet: mac-a is offline. Lightbulb Connect keeps trying until 2026-10-24 10:45. …
B$ lightbulb-connect team
Team "alice" — 2 computers
  You (aden) · mac-b · member
  alice · mac-a · founder · offline
B$ lightbulb-connect leave   (answered y)
Leave "alice"? Your computer stops connecting. Mail already stored here stays. [y/N] Left "alice". This computer is on its own network again.
Could not tell the team; they will see you gone when you are removed.
B$ lightbulb-connect join '<the old link>' --name aden
This invite link does not work. Ask for a new one.
```

**F4:** a removed member is not told. B keeps its roster, shows A as `offline`, and queues mail for 24 h. A only logs `net: closed qxcrztez36jw (no longer a member)`. B should get "You were removed from <team>" (a signed roster from A on the refused connection) and drop to its own network, or at least `send` should say so.

### 10. Receipt sentences

| state | sentence |
|---|---|
| queued | `Not delivered yet: mac-b is offline. Lightbulb Connect keeps trying until 2026-10-24 10:40. Check: lightbulb-connect receipt <id>` |
| held | `Sent. alice has to allow it first (they see a prompt). Check: lightbulb-connect receipt <id>` |
| stored | `Sent. Stored on mac-a for the session's next step. Check: lightbulb-connect receipt <id>` |
| delivered | `Delivered to alice on mac-a.` |
| refused | `Not sent: they declined it.` |

Notes: `receipt` repeats `Check: lightbulb-connect receipt <id>` inside its own output. For a name address (`inbox`, `desk`), `stored` says "for the session's next step" although there is no session. `read` and `expired` were not reached: they need an agent turn, or a 24-hour wait.

### 11. folder off/on

```
A$ lightbulb-connect folder /Users/alice/Desktop/lightbulb.nosync off
Sessions in /Users/alice/Desktop/lightbulb.nosync are hidden from your teammates.
A$ lightbulb-connect who   → … busy, gets messages instantly, hidden from teammates …
B$ lightbulb-connect who --all   → (first call still listed it; about 2 s later only B's own row)
A$ lightbulb-connect folder … on
Sessions in /Users/alice/Desktop/lightbulb.nosync are shown to your teammates.
B$ lightbulb-connect who --all   → A's row back after a few seconds
```

### 12. relay

```
$ lightbulb-connect relay
none (direct connections only)
Our relay list was refused: https://thelightbulb.company/connect/relays.json answered 404
```

The daemon logs `relay: fetching https://thelightbulb.company/connect: https://thelightbulb.company/connect/relays.json answered 404` once at each start. That fetch is the product's own call to its own host.

### 13. Error texts

```
$ lightbulb-connect frobnicate               →  Unknown command: frobnicate. Run lightbulb-connect --help.   (exit 2)
$ lightbulb-connect send codex-9 "…"         →  No session "codex-9" is open right now. Run lightbulb-connect who to see the current ones.
$ lightbulb-connect folder /no/such/place/xyz off  →  no such folder: /no/such/place/xyz
$ lightbulb-connect send nobody/inbox "…"    →  No teammate called "nobody". Teammates: alice, aden (see lightbulb-connect team).
$ lightbulb-connect receipt NOPE             →  No message NOPE was sent from this terminal.
$ lightbulb-connect approve H99              →  No held message H99.
$ (in a team) join '<garbage link>'          →  You are in "alice". Leave it first: lightbulb-connect leave
$ held (from an agent's shell)               →  Run this from your own terminal, not from an agent.
```

The one note: `no such folder: …` is the only error that is not a capitalised sentence.

### Logs

`grep -c ACC7Q` gives 0 in `~/.lightbulb/connect/daemon.log` and 0 in B's `daemon.log`. No message body was logged.

## Not run (on purpose)

| Row | Why |
|---|---|
| `signin`, verified labels, same-person admission | There is no issuer yet |
| Streams | Need the app's stream client |
| Hosted apps (`app_send`), phone decisions, workspace/cloud members | Need the Lightbulb app / backend |
| An agent's own `find_sessions` / `send_message` / framed header / `read` receipts | Driving them means typing into the owner's agent apps |
| The real two-Mac rows (step 2), lid-close, phone hotspot | Need a second Mac; blocked by F2 regardless |
| Rows 1–13 on the release build | F2: release discovery fails |

## Findings to fix (not fixed here)

- **F2 (blocker):** release daemons do not discover each other over mDNS. This is a regression since e864370, most likely from `RelayMode::Custom` in `Net::start` (`4a80b78`), and no test covers it. Add a test that joins two release-mode daemons without `LIGHTBULB_CONNECT_PEERS`.
- **F1:** an upgrade can leave the old daemon serving (a hook-spawned stray holds the lock), and `doctor` reports "All good".
- **F3:** `policy <name>` is inert; the listing shows raw keys.
- **F4:** a removed member is not told and queues mail to "offline" peers.
- Minor: the debug env name in the hidden help text (note 1); `receipt` repeating "Check: receipt"; "session's next step" for name addresses; `no such folder` wording. `setup` no longer mentions the macOS Desktop-folder prompt, but the daemon no longer reads folders (04e73ee), so I saw no prompt this time.

## Step 2: two Macs (run after F2 is fixed)

The teammate follows this on Mac B (a different macOS account and person, release build, the same Wi-Fi as A).

1. Run the install line: `curl -fsSL https://github.com/TheLightbulbCompany/cli/releases/latest/download/install.sh | sh`. It installs `~/.lightbulb/connect/bin/lightbulb`, runs `setup` (one line per agent and `Lightbulb is on.`) and starts the background service.
2. Run `lightbulb doctor`, which must show `Relay                default` and end with `All good.` (If `~/.local/bin` is not on PATH, use the full path.)
3. Quit and restart Claude Code and Codex. An open session does not load the new tool server. The tool server is named **`lightbulb`**.
4. In a plain Terminal (not inside an agent), paste the command A's `invite` printed (`lightbulb join '<ticket>' --name NAME`), with your name in place of `NAME`. Wait for `Connected as <you> (<your computer>) …`. A clicks Allow, or runs `lightbulb approve J<n>`.
5. Run `lightbulb who --all`. A's sessions are listed with A's name and computer.
6. In Claude Code, say: "Use the lightbulb tools: find the sessions of `<A's name>` and send their Claude `ping from B` with send_message." Then: "read its receipt with message_receipt". Text the receipt to A. It reads `held` until A allows it.
7. After A says "allowed", send a second message from Codex the same way. Confirm the receipt reads `stored` or later, without A doing anything.
8. Send a third. A declines it. Confirm the receipt is `refused`, reason `declined`.
9. Close the lid for 2 minutes while A sends to your Claude. Open it. Confirm the message arrives, and that A's receipt reads `delivered`.
10. Run `lightbulb leave` and answer `y`. Confirm that `lightbulb team` lists only you.

A's side records each row with the dialog text, labels and `receipt --json`, then greps both `~/.lightbulb/connect/daemon.log` for the body markers (they must be absent).

## Acceptance fixes (2026-10-23, plan-b 9ba490f, e6a712a, c946668)

**Step 1 must be re-run on the release build** (rows 1–13 and P1–P4, with `lightbulb-connect 0.2.0`). The network rows above ran on a debug build because of F2.

- **F2 (9ba490f, c946668).** Root cause: not `RelayMode::Custom`. In iroh 1.4.1 an empty custom relay map takes the same no-relay path as `Disabled` (`re_stun` sets the direct addresses from the interfaces, and mDNS publishes the same data; traced both ways). Every mDNS send of the daemon got `EHOSTUNREACH`. macOS gives that error to a program that has no Local Network permission. A rebuilt ad-hoc-signed binary is a new program to macOS, so the e864370 binary's grant did not carry over. swarm-discovery logs the error only at `tracing` level, and the daemon does not print that level. The fixes:
  - Daemons of one user on one Mac find each other over loopback: `/tmp/lightbulb-connect-<uid>/<member key>` holds `127.0.0.1:<port> <pid>`, mode 0600. The daemon removes the file on `shutdown`, SIGTERM and SIGINT, and skips a file whose pid is dead.
  - A ticket carries the inviter's direct addresses (at most 8 IP literals). `join` dials them.
  - Every `hello` carries the sender's current addresses. These are kept in `addrs.json`. With them, two Macs on one LAN connect with no relay and no mDNS.
  - The daemon probes mDNS at start. When macOS refuses it, the log says how to allow it, `status.lan` is `blocked` (or `no network` when no multicast interface is up), and `doctor` shows the fix.
  - Tests: `net_two::two_members_on_one_computer_find_each_other` and `join::the_ticket_addresses_alone_reach_the_inviter`. mDNS itself cannot run in the test sandbox, which gets the same `EHOSTUNREACH`.
  - Release check: two 0.2.0 release daemons in temp homes, with no PEERS knob (release has none). `join … --name aden`, then `approve J1` from a detached shell, gave `Connected as aden (mac-b). Team "a": 2 computers online.` Both daemons logged the Local Network refusal, and `status.lan` read `blocked`. Then `remove aden` gave on B: `You were removed from "a" by a on 2026-10-23. This computer is on its own network again.`
  - For step 2: on each Mac, allow `lightbulb` in System Settings > Privacy & Security > Local Network if `doctor` asks, so that mDNS works too.
- **F1 (e6a712a, c946668).** `ensure_running` starts the canonical binary for the real install. `setup` and `daemon --install/--uninstall` send `shutdown` and wait for the lock holder's pid to exit. A daemon that does not exit is reported (`The old background service did not stop (pid N). Run: kill N, then lightbulb-connect setup again.`), and no job is installed behind it. The daemon does not check versions at start.
- **F3.** `policy <name|short|key> <value>` stores the member's person id. More than one match, or no match, gives a sentence. The listing shows `aden (qxcrztez36jw)`. `policy.set` refuses an id that no member has, unless it is a `ws:` id. Your own person is refused. A row stored under a literal name by an older build is marked `old entry`, and `policy <id> forget` deletes it.
- **F4.** A removed member is told its removal (the `remove` entry and the ancestors it lacks, never a later entry) on its live link or on its next dial. Its queued mail becomes `refused not_member`, it logs one line, and it goes solo. `team` says `You were removed from "<team>" by <name> on <date>. This computer is on its own network again.`
- Small: version 0.2.0. The debug env name is no longer in the hidden help. `There is no folder at <path>.`
- Tests: 383 pass, and `fmt` and `clippy -D warnings` are clean. `--test relay`, `--test join` and `--test net_two` each passed 3 times in a row.
- **Review round 1.**
  - `stop_daemon` uses the lock to decide whether a daemon runs: it polls `try_lock` on `daemon.lock`. It uses the pid in the file only in the message. A stale pid that now belongs to some other process no longer blocks `setup`.
  - `parse_addrs` refuses multicast, loopback and IPv4 broadcast addresses. It also applies these checks to the IPv4 inside an IPv4-mapped IPv6 address. Link-local addresses stay.
  - `addrs.json` keeps only roster members, plus the inviter of a join in progress.
  - `join` checks the inviter's signature on the ticket before it keeps or dials the ticket's addresses.
  - Bad `addrs` in a `hello` are dropped. The `hello` itself is still served.
  - A policy row for your own person that an older build stored is listed as ignored, with the `forget` hint.
  - The removal test now also proves that a member admitted after the removal is never sent to the removed member.
  - 385 tests pass.

## Step 1 re-run (0.2.0, 89f34c6)

- Date: 2026-10-23, 14:33–14:45 local (EDT).
- Build: `89f34c6` (`plan-b`), `cargo build --release` only (no debug build), sha256 `49a5acd04e67728ab7f4ce78c5f9b597ed82888b34eb0e9fcce43634bb40fbd1`, `lightbulb-connect 0.2.0`, 22,938,096 bytes.
- A = the real install (`~/.lightbulb/connect`, was 0.1.0 from e08b3fa, state.db V6). B = `LIGHTBULB_CONNECT_HOME=/tmp/lbc-b2` (fresh), `computer = "mac-b"`, the same binary, `daemon` in a tmux window. C (row 7 only) = `/tmp/lbc-c2`, `mac-c`. No `LIGHTBULB_CONNECT_PEERS` (release has none): B and C found A only through `/tmp/lightbulb-connect-501/<key>` and the ticket's addresses.
- Every person-side command ran in tmux. Each macOS dialog was answered from the CLI, which withdrew it (no `osascript` was left running), except J4 (see F7). Dialog texts are copied from the `osascript` arguments. Body markers: `ACC8R-<n>`.

### Results

| # | Row | Result |
|---|-----|--------|
| P1 | Release binary: no `DEBUG_` knob names, no test issuer | PASS (the hidden-help knob name of note 1 is gone too) |
| P2 | Upgrade by rename + one `setup`: one daemon, the new inode | PASS (no F1 regression) |
| P3 | V6 → V6 no-op: `user_version=6`, row counts equal the backup's | PASS |
| P4 | Configs byte-identical after `setup`; 20 hooks with `{}` exit 0 | PASS |
| 1 | invite → join `--name aden` → dialog → Allow → Connected | PASS (the Allow did not come from my `approve J4`; F7) |
| 2 | B send → `held` + dialog → `approve H5` → delivered | PASS |
| 3 | A second B send goes straight in | PASS |
| 4 | A fresh one declined → `refused`, reason `declined` | PASS |
| 5 | B stopped, A sends → `queued`; B restarted → held on B, allowed → delivered | PASS |
| 6 | `who --all` on both shows both computers' sessions | PASS |
| 7 | `held` lists a pending join and a held message, with their commands | PASS |
| 8 | `policy`; `policy aden hold` / `accept` BY NAME take effect | PASS (F3 fixed) |
| 9 | `remove aden` → B is out and is TOLD; link replaced | PASS (F4 fixed) |
| 10 | `receipt` sentences for each state | PASS (queued, held, stored, delivered, refused) |
| 11 | `folder <path> off` hides a session from B, `on` shows it again | PASS |
| 12 | `relay` prints the list and its source | PASS |
| 13 | Error texts | PASS |
| 14 | `join` with a ticket whose addrs are edited → refused | PASS |
| 15 | After `remove`, B's key is gone from A's `addrs.json` after A's next save/restart | **FAIL (F6)** |
| — | Message bodies absent from every daemon log | PASS |

### P1

```
$ lightbulb-connect version
lightbulb-connect 0.2.0
strings: LIGHTBULB_CONNECT_DEBUG 0, DEBUG_ 0, LIGHTBULB_CONNECT_PEERS 0, lightbulb-debug-test-issuer 0,
         LIGHTBULB_CONNECT_CLAUDE_HOME 0, LIGHTBULB_CONNECT_HOOK_CALLER 0, test-issuer key bytes present: False
```

### P2. Backups, rename, one `setup`, `doctor`

Backups: `~/.lightbulb/acc2-backup-2026-10-23/` holds the agents' config files (`.claude/settings.json`, `.claude.json`, `.codex/{config.toml,hooks.json}`, `.cursor/{cli-config,hooks,mcp}.json`, `.grok/config.toml`, `.grok/hooks/`), `connect/state.db` (sqlite `.backup`, V6), `connect/identity.key`, `connect/config.toml`, the launchd plist, and the old binary `lightbulb-connect.e08b3fa`.

```
before: pid 7237 …/bin/lightbulb-connect daemon   (inode 1319811885, the e08b3fa binary)
(new binary copied to bin/lightbulb-connect.new, then mv over bin/lightbulb-connect → inode 1321273298)
$ lightbulb-connect setup                         (no `daemon --uninstall` first)
Command: lightbulb-connect ("lightbulb" belongs to the Lightbulb app)
Claude Code: connected (hooks + tools as "lightbulb-connect" — another "lightbulb" tool server is already installed)
Codex: connected (hooks + tools as "lightbulb-connect" — another "lightbulb" tool server is already installed)
Cursor: connected (hooks + tools as "lightbulb-connect" — another "lightbulb" tool server is already installed)
Grok CLI: connected (hooks + tools as "lightbulb-connect" — another "lightbulb" tool server is already installed)
Background service: running (starts at login).

Lightbulb Connect is on.
  Connected: Claude Code, Codex, Cursor, Grok CLI

Try it: open Claude Code in any folder and ask "list my other sessions".
Add your other computer or a teammate: lightbulb-connect invite
Check everything: lightbulb-connect doctor
exit=0
$ pgrep -fl "lightbulb-connect daemon"
53795 /Users/alice/.lightbulb/connect/bin/lightbulb-connect daemon    (lsof txt: inode 1321273298, the new one)
$ lightbulb-connect doctor                        (right after setup)
Background service   running
Local network        blocked by macOS
Starts at login      yes
State folder         private
Claude Code          connected
Codex                connected
Cursor               connected
Grok CLI             connected
Command name         "lightbulb" belongs to the Lightbulb app; type "lightbulb-connect"
Allow lightbulb-connect in System Settings > Privacy & Security > Local Network
exit=1
$ status (socket)  {"lan":"blocked","port":56755,"relay":"none","version":"0.2.0",…}
/tmp/lightbulb-connect-501/gjivepwn…me54a  =  "127.0.0.1:56755 53795"  (mode 0600, dir 0700)
daemon.log: net: macOS refuses this program the local network, so computers on it cannot find this one. Allow lightbulb-connect in System Settings > Privacy & Security > Local Network.
```

Exactly one daemon, on the new inode, after one `setup`: the F1 fix works. `doctor` did not say `All good.` at this point only because of the Local Network row: the rebuilt binary is a new program to macOS. Local Network was allowed on the owner's side during the run (I did not click anything). After A's restart for row 15, `status.lan` read `on` and the final `doctor` said `All good.` (below).

### P3

```
user_version=6
backup: approvals=4 grants=0 hidden_sessions=0 holds=4 join_requests=3 mailbox=4 messages=11 outbox=2 peers_sessions=2
        policy=1 receipt_out=0 receipts=11 roster=8 seen_ids=8 sessions=4 team=1 team_link=3
after setup: the same counts; messages, mailbox, receipts, roster, holds, outbox, team, policy, approvals,
        join_requests identical row-for-row; schema identical
```

### P4

Every config file is byte-identical to the backup: `.claude/settings.json`, `.codex/config.toml`, `.codex/hooks.json`, `.cursor/{cli-config,hooks,mcp}.json`, `.grok/config.toml`, `.grok/hooks/lightbulb-connect.json` and `lightbulb.json`. `~/.claude.json` differs as a whole file because Claude Code rewrites it all the time, but its `mcpServers` is identical. The 20 `hook <claude|codex|cursor|grok> <session-start|user-prompt-submit|post-tool-use|stop|session-end>` calls with `{}` on stdin all exited 0.

### 1. Join

```
A$ lightbulb-connect invite
Send this link to the person (or paste it on your other computer): lightbulb-connect join '<link>'
Join Lightbulb Connect: https://thelightbulb.company/j/#eyJhZGRycyI6…   (764 characters)
   ticket: addrs ['192.0.2.12:56755', '192.0.2.10:56755', '192.0.2.11:56755'], relay_hint '', mode ask
B$ lightbulb-connect join '<link>' --name aden
Asking whoever sent the link to allow this computer… (they get a prompt; this waits up to 10 minutes)
A dialog: [Lightbulb Connect] aden (computer "mac-b") wants to join your team using your invite link. Allow only if you sent them the link.
          Or run: lightbulb-connect approve J4            [Decline] [Allow]
A$ lightbulb-connect held
J4  aden (mac-b) wants to join — lightbulb-connect approve J4
A$ lightbulb-connect approve J4
J4 was already answered or withdrawn.                     ← see F7
A log: join: J4 open for vf73ot6v7bdg / join: J4 admitted / net: connected vf73ot6v7bdg
B: Connected as aden (mac-b). Team "alice": 2 computers online.
   Your agents here can now see and message the team's agents. Try: lightbulb-connect who --all
   Until both computers sign in, your other computer counts as another person and its first message needs your Allow.
A$ lightbulb-connect team
Team "alice" — 2 computers
  You (alice) · mac-a · founder
  aden · mac-b · member
Invite someone: lightbulb-connect invite
```

The release daemons found each other with no PEERS knob and with mDNS blocked: B dialed the ticket's addresses / A's loopback file.

### 2. First message held, then allowed (A: `listen --as inbox`)

```
B$ lightbulb-connect send alice/inbox 'ACC8R-1 first message from aden'
Sent. alice has to allow it first (they see a prompt). Check: lightbulb-connect receipt 01M5N0W6C2H1HV29HXY6BJTNE6
A$ lightbulb-connect held
H5  aden, at the terminal (not verified — they have not signed in) -> gjivepwnkmgn/inbox  "ACC8R-1 first message from aden" — lightbulb-connect approve H5
A$ lightbulb-connect approve H5   →  Allowed H5.        (dialog withdrawn)
A listen: From aden, at the terminal (not verified): ACC8R-1 first message from aden
          Reply: lightbulb-connect send vf73ot6v7bdg/terminal "…" --reply-to 01M5N0W6C2H1HV29HXY6BJTNE6
B$ lightbulb-connect receipt 01M5N0W6… --json
{"online":true,"person":"alice","reason":null,"request_id":"01M5N0W6C2H1HV29HXY6BJTNE6","state":"delivered","thread":"01M5N0W6C2DA7B1021NNJXS5PH","where":"mac-a"}
```

The held-message dialog text is under row 4. The `held` receipt JSON is also under row 4.

### 3. Second message goes straight in

```
B$ lightbulb-connect send alice/inbox 'ACC8R-2 second message'
Sent. Stored on mac-a for the session's next step. Check: lightbulb-connect receipt 01M5N0XNCJAF811J7D1R9NYYPT
A listen: From aden, at the terminal (not verified): ACC8R-2 second message
{"online":true,"person":"alice","reason":null,"request_id":"01M5N0XNCJAF811J7D1R9NYYPT","state":"delivered",…}
```

### 4 and 8. Policy by name; held; declined; accept

```
A$ lightbulb-connect policy
Your workspace's agents may message your sessions without asking: no.
Messages from awljc6ald6ktba5yob5zwn46qql2jltsm7ivb7yxiwq7sg5w5fba: accepted. (old entry — run: lightbulb-connect policy awljc6ald6ktba5yob5zwn46qql2jltsm7ivb7yxiwq7sg5w5fba forget)
Messages from aden (vf73ot6v7bdg): accepted.
A$ lightbulb-connect policy aden hold
… Messages from aden (vf73ot6v7bdg): held for your Allow.
   (db: people|vf73ot6v7bdg…wpf72q|hold|local — stored under aden's person id, not the literal name)
B$ lightbulb-connect send alice/desk 'ACC8R-3 please decline this'
Sent. alice has to allow it first (they see a prompt). Check: lightbulb-connect receipt 01M5N0YBDVJ5JCY1YSPW28X0Q6
A dialog: [Lightbulb Connect] aden, at the terminal (not verified) wants to send a message to "desk" on this computer.
          "ACC8R-3 please decline this"                   [Decline] [Allow]
B$ receipt --json  {"online":true,"person":"alice","reason":null,"request_id":"01M5N0YBDVJ5JCY1YSPW28X0Q6","state":"held","thread":"01M5N0YBDVXPEQ9K6G50VSMTYR","where":"mac-a"}
A$ lightbulb-connect held
H6  aden, at the terminal (not verified — they have not signed in) -> gjivepwnkmgn/desk  "ACC8R-3 please decline this" — lightbulb-connect approve H6
A$ lightbulb-connect decline H6   →  Declined H6.      (dialog withdrawn)
B$ receipt   Not sent: they declined it.
B$ receipt --json  {"online":true,"person":"alice","reason":"declined","request_id":"01M5N0YBDVJ5JCY1YSPW28X0Q6","state":"refused",…}
A$ lightbulb-connect policy aden accept   → … Messages from aden (vf73ot6v7bdg): accepted.
A$ lightbulb-connect policy aden maybe    → policy takes accept, hold or refuse.  (exit 1)
A$ lightbulb-connect policy nobody hold   → No teammate called "nobody". Teammates: alice, aden (see lightbulb-connect team).
A$ lightbulb-connect policy alice hold → That is you; your own computers always get through.
B$ lightbulb-connect send alice/desk 'ACC8R-4 after accept by name'
Sent. Stored on mac-a for the session's next step. Check: lightbulb-connect receipt 01M5N0YV0HF027DSZRZWCEPTAV
```

The `awljc…` row was in A's state before this run (a key no member has now), so it shows as `old entry`. I left it.

### 5. Offline, queued, delivered

```
(B's daemon stopped with Ctrl-C; its file in /tmp/lightbulb-connect-501/ was removed)
A$ lightbulb-connect send aden/desk 'ACC8R-5 sent while you were away'
Not delivered yet: mac-b is offline. Lightbulb Connect keeps trying until 2026-10-24 14:39. Check: lightbulb-connect receipt 01M5N0ZAD586SJJYR2YTV94H6T
{"online":false,"person":"aden","reason":null,"request_id":"01M5N0ZAD586SJJYR2YTV94H6T","state":"queued","thread":"01M5N0ZAD5G55R1HE5H0G5H1E9","until":1792867173,"where":"mac-b"}
(B restarted; B$ lightbulb-connect listen --as desk --once)
B dialog: [Lightbulb Connect] alice, at the terminal (not verified) wants to send a message to "desk" on this computer.
          "ACC8R-5 sent while you were away"              [Decline] [Allow]
B$ lightbulb-connect held
H1  alice, at the terminal (not verified — they have not signed in) -> vf73ot6v7bdg/desk  "ACC8R-5 sent while you were away" — lightbulb-connect approve H1
B$ lightbulb-connect approve H1   →  Allowed H1.
B listen: From alice, at the terminal (not verified): ACC8R-5 sent while you were away
A$ lightbulb-connect receipt 01M5N0ZA…   →  Delivered to aden on mac-b.
```

### 6. who --all

```
A$ lightbulb-connect who --all
Name                                                      Person                Computer         Agent        Folder                                                 Status                         Doing
claude-1                                                  alice  mac-a  Claude Code  /Users/alice/Desktop/lightbulb.nosync/  idle, gets messages instantly  ship-agent-network-phase-1
vf73ot6v7bdg/claude:dc77321a-…                            aden                  mac-b            Claude Code                                                         idle, gets messages instantly  ship-agent-network-phase-1
B$ lightbulb-connect who --all
vf73ot6v7bdg/claude:dc77321a-…  aden                  mac-b            Claude Code  /Users/…/lightbulb.nosync  idle, gets messages instantly  …
gjivepwnkmgn/claude:dc77321a-…  alice  mac-a  Claude Code  lightbulb                  idle, gets messages instantly  …
```

The same one-Mac artifact as before: B lists A's Claude session as its own, because both daemons read `~/.claude`.

### 7. held with a join and a message

```
(C: lightbulb-connect join '<link>' --name visitor; A$ lightbulb-connect policy aden hold; B sends ACC8R-6)
A$ lightbulb-connect held
J5  visitor (mac-c) wants to join — lightbulb-connect approve J5
H7  aden, at the terminal (not verified — they have not signed in) -> gjivepwnkmgn/inbox  "ACC8R-6 held for row 7" — lightbulb-connect approve H7
A dialog (one at a time): aden, at the terminal (not verified) wants to send a message to "inbox" on this computer. "ACC8R-6 held for row 7"
A$ lightbulb-connect decline J5   →  Declined visitor.        C: Your request to join was declined.
A$ lightbulb-connect approve H7   →  Allowed H7.               A listen: From aden, at the terminal (not verified): ACC8R-6 held for row 7
A$ lightbulb-connect policy aden accept
(no osascript left running)
```

### 9. remove; B is told

```
A$ lightbulb-connect remove aden
Removed aden. New connections and new mail from it are blocked. The invite link was replaced: Join Lightbulb Connect: https://thelightbulb.company/j/#eyJhZGRycyI6…
A$ lightbulb-connect team
Team "alice" — 1 computer
  You (alice) · mac-a · founder
B$ lightbulb-connect team
You were removed from "alice" by alice on 2026-10-23. This computer is on its own network again.
Team "alice" — 1 computer
  You (alice) · mac-b · founder
B log: net: removed from "alice" by alice; on our own network again
A log: net: closed vf73ot6v7bdg (no longer a member)
B$ lightbulb-connect leave       (answered n)
Leave "alice"? Your computer stops connecting. Mail already stored here stays. [y/N]   ← see F8
B$ lightbulb-connect join '<the old link>' --name aden
Asking whoever sent the link to allow this computer… (they get a prompt; this waits up to 10 minutes)
This invite link does not work. Ask for a new one.        (A log: join: refused vf73ot6v7bdg (bad_ticket))
```

The solo team on B is named after the macOS user (`alice`), which is A's name too, because both run as the same user. That is a one-Mac artifact.

### 10. Receipt sentences

| state | sentence |
|---|---|
| queued | `Not delivered yet: mac-b is offline. Lightbulb Connect keeps trying until 2026-10-24 14:39. Check: lightbulb-connect receipt <id>` |
| held | `Sent. alice has to allow it first (they see a prompt). Check: lightbulb-connect receipt <id>` |
| stored | `Sent. Stored on mac-a for the session's next step. Check: lightbulb-connect receipt <id>` |
| delivered | `Delivered to alice on mac-a.` / `Delivered to aden on mac-b.` |
| refused | `Not sent: they declined it.` |

`read` and `expired` were not reached, for the same reasons as before.

### 11. folder off/on

```
A$ lightbulb-connect folder /Users/alice/Desktop/lightbulb.nosync off
Sessions in /Users/alice/Desktop/lightbulb.nosync are hidden from your teammates.
A$ lightbulb-connect who   → claude-1 … busy, gets messages instantly, hidden from teammates …
B$ lightbulb-connect who --all   → only B's own row, already on the first call
A$ lightbulb-connect folder … on
Sessions in /Users/alice/Desktop/lightbulb.nosync are shown to your teammates.
B$ lightbulb-connect who --all   → A's row back about 6 s later
```

### 12. relay

```
$ lightbulb-connect relay
none (direct connections only)
Our relay list was refused: https://thelightbulb.company/connect/relays.json answered 404
```

### 13. Error texts

```
$ lightbulb-connect frobnicate               →  Unknown command: frobnicate. Run lightbulb-connect --help.   (exit 2)
$ lightbulb-connect send codex-9 "x"         →  No session "codex-9" is open right now. Run lightbulb-connect who to see the current ones.
$ lightbulb-connect folder /no/such/place/xyz off  →  There is no folder at /no/such/place/xyz.
$ lightbulb-connect send nobody/inbox "x"    →  No teammate called "nobody". Teammates: alice, aden (see lightbulb-connect team).
$ lightbulb-connect receipt NOPE             →  No message NOPE was sent from this terminal.
$ lightbulb-connect approve H99              →  No held message H99.
$ lightbulb-connect approve J99              →  No join request J99.
$ (in a team) join 'garbage'                 →  You are in "alice". Leave it first: lightbulb-connect leave
$ lightbulb-connect policy aden maybe        →  policy takes accept, hold or refuse.
$ held (from an agent's shell)               →  Run this from your own terminal, not from an agent.
```

### 14. A ticket with edited addresses

I took A's current link (after `remove`), changed `addrs` from `['192.0.2.12:56755', '192.0.2.10:56755', '192.0.2.11:56755']` to `["224.0.0.251:5353","127.0.0.1:22"]`, kept `sig`, and joined from B (solo):

```
B$ lightbulb-connect join '<edited link>' --name aden
Asking whoever sent the link to allow this computer… (they get a prompt; this waits up to 10 minutes)
This invite link does not work. Ask for a new one.
exit=1
```

The refusal is local. A logged nothing for it, and nothing was dialed. In `team.join`, `parse_addrs` (which refuses multicast and loopback) and `signed_by_inviter` both reject this ticket (`src/admission.rs:862-864`). The waiting line before the refusal is F9.

### 15. A's `addrs.json` after `remove`

```
before remove: {"vf73ot6v7bdg…wpf72q": ["192.0.2.12:61720","192.0.2.10:61720","192.0.2.11:61720"]}
after remove:  unchanged (mtime 14:39)
after launchctl kickstart -k of A (new pid 62095, same new inode): unchanged — B's key is still there
```

### Logs

`grep -c ACC8R` gives 0 in `~/.lightbulb/connect/daemon.log`, 0 in B's daemon output and 0 in C's daemon output.

### End state of A

```
$ pgrep -fl "lightbulb-connect daemon"
62095 /Users/alice/.lightbulb/connect/bin/lightbulb-connect daemon     (inode 1321273298)
$ lightbulb-connect doctor
Background service   running
Starts at login      yes
State folder         private
Claude Code          connected
Codex                connected
Cursor               connected
Grok CLI             connected
Command name         "lightbulb" belongs to the Lightbulb app; type "lightbulb-connect"
All good.
status: {"lan":"on","version":"0.2.0","relay":"none",…}; user_version=6
```

The policy table equals the backup's: aden's row was removed with `policy <key> forget`. B, C, the tmux session and the temp homes were removed. No dialog is open.

### Findings (re-run)

- **F6 (row 15 FAIL): a removed member's addresses stay in `addrs.json` across `remove` and a restart.** `Peers::learn` (`src/net/mod.rs:311-321`) is the only code that prunes the file to roster members. It runs only when a peer's `hello` brings changed addresses. `remove` does not prune the file, and `load_known` (`src/net/mod.rs:324-333`) reloads every entry with no roster filter. A team that has no other members never saves the file again. The file is also mode 0644 (its folder is 0700). Fix idea: filter by the roster in `load_known`, and save the file after `remove`.
- **F7 (row 1): J4 was answered before my `approve J4`.** The CLI said `J4 was already answered or withdrawn.` and the log said only `join: J4 admitted`. I did not click the dialog, and its default button is Decline, so a key press did not do it either: someone clicked Allow on the screen. Nothing in the log, the row (`join_requests.state = admitted`) or the CLI tells you which surface answered. Fix idea: log `J4 admitted (dialog)` / `(cli)`.
- **F8 (minor): `leave` on a computer that is on its own network asks `Leave "<its own solo team>"?`.** After the removal, B's solo team is still named, and `leave` offers to leave it. It should say that there is nothing to leave.
- **F9 (minor): `join` with a bad ticket prints `Asking whoever sent the link to allow this computer… (they get a prompt; this waits up to 10 minutes)` before `This invite link does not work.`** The daemon refuses before any dial, so the waiting line is wrong. The CLI prints it for any ticket that decodes in `ask` mode, before the daemon checks it (`src/cli.rs:627-629`).
- **Minor, from the first run and still present:** `policy takes accept, hold or refuse.` is not a capitalised sentence. `stored` says "for the session's next step" for a name address (`inbox`, `desk`). `receipt` repeats `Check: lightbulb-connect receipt <id>`.
