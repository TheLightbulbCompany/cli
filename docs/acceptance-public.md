# Acceptance on the public release (2026-10-26)

Two computers: a Mac and a Linux (WSL) machine on one network, plus a workspace whose agents
joined it. Every row ran against the live service. A row passes only with the evidence noted.

| # | Check | Version | Result | Evidence |
|---|---|---|---|---|
| A1 | Join: invite on the Mac, join on WSL, allow | 0.5.0 | Pass | `team` lists both computers; `who --all` on each lists the other's sessions |
| A2 | Agents across computers (Claude Code, Codex, OpenCode) | 0.5.0 | Pass | replies delivered both ways |
| A3 | `lightbulb login` on both | 0.5.0 | Pass | `doctor`: "Lightbulb sign-in signed in" |
| A4 | Hosted MCP from claude.ai (custom connector, OAuth) | 0.5.0 | Pass | find_sessions listed both computers; a send to OpenCode on WSL got its reply on the first check_inbox, same thread |
| A5 | ChatGPT and Muse | — | Not run | ChatGPT Chat has no custom MCP in the Mac app; needs the web Plugin Directory |
| A6 | Offline route | 0.5.0 | Pass | app pinned to a stopped computer: `NETWORK_OFFLINE: <computer> is offline. Open it.`, no hang. Stopping the service on a computer with open agent sessions is not an offline test: their `lightbulb mcp` restarts the daemon |
| B8-9 | Workspace leaves its network; `invite --agents` | 0.5.0 | Pass | browser confirm; `team` shows the workspace; its agents listed on both computers |
| B10 | Workspace agent answers members | 0.5.0 / 0.6.0 | Pass | replies to the Mac (held once on 0.5.0) and to OpenCode on WSL (no hold on 0.6.0) |
| B11 | Signed-out computer is refused; sign in again, answered | 0.6.3 | Pass | after `logout`: receipt `refused`, reason `not_a_member`; after `login`, no restart: reply arrived |
| R1 | One MCP named `lightbulb`; one `lightbulb` command | 0.6.1 | Pass | agent configs hold one `lightbulb` server (the engine); `lightbulb claude --version` reaches the app |
| R2 | Linux `lightbulb update` checks the signed checksum | 0.6.0 | Pass | update from 0.5.0 installed; release carries `.sha256.sig` |

## Bugs found and fixed during this run

- Linux: a process's start time followed the wall clock, so after a clock step (WSL) every tool call
  answered `source_unidentified`. Births now count from boot (0.6.1).
- `logout` did not reach the workspace, and a new `login` stayed refused until a restart. Logout now
  signs the computer out at Lightbulb and both re-announce the proof (0.6.2, 0.6.3).
- The workspace bundled an engine that ignored revocations; it now runs 0.6.2 or later.
