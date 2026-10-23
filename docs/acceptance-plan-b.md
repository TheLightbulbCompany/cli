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
B: Connected as aden (mac-b). Team "prasiddhaparthsarthy": 2 computers online.
   Your agents here can now see and message the team's agents. Try: lightbulb-connect who --all
   Until both computers sign in, your other computer counts as another person and its first message needs your Allow.
A$ lightbulb-connect team
Team "prasiddhaparthsarthy" — 2 computers
  You (prasiddhaparthsarthy) · PPs-MacBook-Pro · founder
  aden · mac-b · member
```

### 2. First message held, then allowed

A ran `lightbulb-connect listen --as inbox`. (I did not use A's Claude session as the target: it was this live agent conversation.)

```
B$ lightbulb-connect send prasiddhaparthsarthy/inbox "ACC7Q-1 first message from aden"
Sent. prasiddhaparthsarthy has to allow it first (they see a prompt). Check: lightbulb-connect receipt 01M5MK84VVVV47KTR5FEBKZW1P
A dialog: aden, at the terminal (not verified) wants to send a message to "inbox" on this computer.
          "ACC7Q-1 first message from aden"              [Decline] [Allow]
A$ lightbulb-connect held
H2  aden, at the terminal (not verified — they have not signed in) -> gjivepwnkmgn/inbox  "ACC7Q-1 first message from aden" — lightbulb-connect approve H2
B$ lightbulb-connect receipt 01M5MK84… --json
{"online":true,"person":"prasiddhaparthsarthy","reason":null,"request_id":"01M5MK84VVVV47KTR5FEBKZW1P","state":"held","thread":"01M5MK84VWA6H9VVR0FCR9MEW4","where":"PPs-MacBook-Pro"}
A$ lightbulb-connect approve H2
Allowed H2.
A listen: From aden, at the terminal (not verified): ACC7Q-1 first message from aden
          Reply: lightbulb-connect send qxcrztez36jw/terminal "…" --reply-to 01M5MK84VVVV47KTR5FEBKZW1P
B$ lightbulb-connect receipt 01M5MK84…
Delivered to prasiddhaparthsarthy on PPs-MacBook-Pro.
```

The sender label is `aden, at the terminal (not verified)`. I did not observe an agent's framed header (`[Lightbulb teammate contribution from <label> {"requestId":…}]`, `src/frame.rs:6`) in this pass, because nothing was pushed to a real agent session.

### 3. Second message goes straight in

```
B$ lightbulb-connect send prasiddhaparthsarthy/inbox "ACC7Q-2 second message"
Sent. Stored on PPs-MacBook-Pro for the session's next step. Check: lightbulb-connect receipt 01M5MK8GTYVQYKZPE1K6RK6SAN
A listen: From aden, at the terminal (not verified): ACC7Q-2 second message
B$ lightbulb-connect receipt 01M5MK8G…       →  Delivered to prasiddhaparthsarthy on PPs-MacBook-Pro.
```

(The Allow covers the person for every name on A: a send to `desk` also went straight in.)

### 4. Declined

To get a fresh hold, I set A's policy for B's key to `hold` (see F3 for why it had to be the key).

```
B$ lightbulb-connect send prasiddhaparthsarthy/desk "ACC7Q-5 please decline this"
Sent. prasiddhaparthsarthy has to allow it first (they see a prompt). Check: lightbulb-connect receipt 01M5MK9H42WKR7QPB6ZEZZN3JV
A$ lightbulb-connect decline H3
Declined H3.
B$ lightbulb-connect receipt 01M5MK9H…
Not sent: they declined it.
{"online":true,"person":"prasiddhaparthsarthy","reason":"declined","request_id":"01M5MK9H42WKR7QPB6ZEZZN3JV","state":"refused",…}
```

### 5. Offline, queued, delivered

```
(B's daemon stopped)
A$ lightbulb-connect send aden/desk "ACC7Q-7 sent while you were away"
Not delivered yet: mac-b is offline. Lightbulb Connect keeps trying until 2026-10-24 10:40. Check: lightbulb-connect receipt 01M5MKABV9RMXWPKAQSMKX22TK
{"online":false,"person":"aden","reason":null,…,"state":"queued","until":1792852855,"where":"mac-b"}
(B restarted; B$ lightbulb-connect listen --as desk --once)
B dialog: prasiddhaparthsarthy, at the terminal (not verified) wants to send a message to "desk" on this computer. "ACC7Q-7 sent while you were away"
B$ lightbulb-connect approve H1   →  Allowed H1.
B listen: From prasiddhaparthsarthy, at the terminal (not verified): ACC7Q-7 sent while you were away
A$ lightbulb-connect receipt 01M5MKAB…   →  Delivered to aden on mac-b.
```

### 6. who --all

```
A$ lightbulb-connect who --all
Name                                                      Person                Computer         Agent        Folder                                                 Status                         Doing
claude-1                                                  prasiddhaparthsarthy  PPs-MacBook-Pro  Claude Code  /Users/prasiddhaparthsarthy/Desktop/lightbulb.nosync/  idle, gets messages instantly  ship-agent-network-phase-1
qxcrztez36jw/claude:dc77321a-…                            aden                  mac-b            Claude Code                                                         idle, gets messages instantly  ship-agent-network-phase-1
B$ lightbulb-connect who --all
qxcrztez36jw/claude:dc77321a-…  aden                  mac-b            Claude Code  /Users/…/lightbulb.nosync  idle, gets messages instantly  …
gjivepwnkmgn/claude:dc77321a-…  prasiddhaparthsarthy  PPs-MacBook-Pro  Claude Code  lightbulb                  idle, gets messages instantly  …
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
B$ send … "ACC7Q-4 …"   →  Sent. Stored on PPs-MacBook-Pro …              ← NOT held
A$ lightbulb-connect policy qxcrztez36jw…rhfa hold   → the next send was held (row 4)
A$ lightbulb-connect policy qxcrztez36jw…rhfa accept
B$ send … "ACC7Q-6 after accept"   →  Sent. Stored on PPs-MacBook-Pro for the session's next step. …
A$ lightbulb-connect policy aden maybe   →  policy takes accept, hold or refuse.  (exit 1)
```

**F3: `policy <name> <value>` is silently inert.** It stores `person_id = "aden"` literally (`policy` table: `people|aden|hold|local|…`). `policy::decide` looks up the member's person id, which for an unverified member is its key. The CLI accepts any string and echoes it back as if it worked. The listing also prints raw 52-character keys instead of names. Fix idea: resolve a member name to its person id in `policy`, as `send` does, refuse names it cannot resolve, and print names in the listing. (I deleted the two policy rows I created from A's db at the end.)

### 9. remove and leave — F4

```
A$ lightbulb-connect remove aden
Removed aden. New connections and new mail from it are blocked. The invite link was replaced: Join Lightbulb Connect: https://thelightbulb.company/j/#eyJ…
A$ lightbulb-connect team   →  Team "prasiddhaparthsarthy" — 1 computer
B$ lightbulb-connect send prasiddhaparthsarthy/inbox "ACC7Q-11 after removal"
Not delivered yet: PPs-MacBook-Pro is offline. Lightbulb Connect keeps trying until 2026-10-24 10:45. …
B$ lightbulb-connect team
Team "prasiddhaparthsarthy" — 2 computers
  You (aden) · mac-b · member
  prasiddhaparthsarthy · PPs-MacBook-Pro · founder · offline
B$ lightbulb-connect leave   (answered y)
Leave "prasiddhaparthsarthy"? Your computer stops connecting. Mail already stored here stays. [y/N] Left "prasiddhaparthsarthy". This computer is on its own network again.
Could not tell the team; they will see you gone when you are removed.
B$ lightbulb-connect join '<the old link>' --name aden
This invite link does not work. Ask for a new one.
```

**F4:** a removed member is not told. B keeps its roster, shows A as `offline`, and queues mail for 24 h. A only logs `net: closed qxcrztez36jw (no longer a member)`. B should get "You were removed from <team>" (a signed roster from A on the refused connection) and drop to its own network, or at least `send` should say so.

### 10. Receipt sentences

| state | sentence |
|---|---|
| queued | `Not delivered yet: mac-b is offline. Lightbulb Connect keeps trying until 2026-10-24 10:40. Check: lightbulb-connect receipt <id>` |
| held | `Sent. prasiddhaparthsarthy has to allow it first (they see a prompt). Check: lightbulb-connect receipt <id>` |
| stored | `Sent. Stored on PPs-MacBook-Pro for the session's next step. Check: lightbulb-connect receipt <id>` |
| delivered | `Delivered to prasiddhaparthsarthy on PPs-MacBook-Pro.` |
| refused | `Not sent: they declined it.` |

Notes: `receipt` repeats `Check: lightbulb-connect receipt <id>` inside its own output. For a name address (`inbox`, `desk`), `stored` says "for the session's next step" although there is no session. `read` and `expired` were not reached: they need an agent turn, or a 24-hour wait.

### 11. folder off/on

```
A$ lightbulb-connect folder /Users/prasiddhaparthsarthy/Desktop/lightbulb.nosync off
Sessions in /Users/prasiddhaparthsarthy/Desktop/lightbulb.nosync are hidden from your teammates.
A$ lightbulb-connect who   → … busy, gets messages instantly, hidden from teammates …
B$ lightbulb-connect who --all   → (first call still listed it; about 2 s later only B's own row)
A$ lightbulb-connect folder … on
Sessions in /Users/prasiddhaparthsarthy/Desktop/lightbulb.nosync are shown to your teammates.
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
$ lightbulb-connect send nobody/inbox "…"    →  No teammate called "nobody". Teammates: prasiddhaparthsarthy, aden (see lightbulb-connect team).
$ lightbulb-connect receipt NOPE             →  No message NOPE was sent from this terminal.
$ lightbulb-connect approve H99              →  No held message H99.
$ (in a team) join '<garbage link>'          →  You are in "prasiddhaparthsarthy". Leave it first: lightbulb-connect leave
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

1. Get the `lightbulb-connect` binary built from the same commit. Run `mkdir -p ~/.lightbulb/connect/bin`, copy the binary to `~/.lightbulb/connect/bin/lightbulb-connect`, then `chmod 755` it.
2. Run `~/.lightbulb/connect/bin/lightbulb-connect setup`. It prints one line per agent and `Lightbulb Connect is on.` Then run `lightbulb-connect doctor`, which must end with `All good.` (If `~/.local/bin` is not on PATH, use the full path.)
3. Quit and restart Claude Code and Codex. An open session does not load the new tool server. The tool server is named **`lightbulb-connect`**.
4. In a plain Terminal (not inside an agent), run `lightbulb-connect join '<the line A sent you>' --name <your name>`. Wait for `Connected as <you> (<your computer>) …`. A clicks Allow, or runs `lightbulb-connect approve J<n>`.
5. Run `lightbulb-connect who --all`. A's sessions are listed with A's name and computer.
6. In Claude Code, say: "Use the lightbulb-connect tools: find the sessions of `<A's name>` and send their Claude `ping from B` with send_message." Then: "read its receipt with message_receipt". Text the receipt to A. It reads `held` until A allows it.
7. After A says "allowed", send a second message from Codex the same way. Confirm the receipt reads `stored` or later, without A doing anything.
8. Send a third. A declines it. Confirm the receipt is `refused`, reason `declined`.
9. Close the lid for 2 minutes while A sends to your Claude. Open it. Confirm the message arrives, and that A's receipt reads `delivered`.
10. Run `lightbulb-connect leave` and answer `y`. Confirm that `lightbulb-connect team` lists only you.

A's side records each row with the dialog text, labels and `receipt --json`, then greps both `~/.lightbulb/connect/daemon.log` for the body markers (they must be absent).
