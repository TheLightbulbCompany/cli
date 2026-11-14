# Acceptance on the public release (2026-10-26)

Two computers: a Mac and a Linux (WSL) machine on one network, on the public release. Every row ran against the live service. A row passes only with the evidence noted.

| # | Check | Version | Result | Evidence |
|---|---|---|---|---|
| A1 | Join: invite on the Mac, join on WSL, allow | 0.5.0 | Pass | `team` lists both computers; `who --all` on each lists the other's sessions |
| A2 | Agents across computers (Claude Code, Codex, OpenCode) | 0.5.0 | Pass | replies delivered both ways |
| A6 | Offline route | 0.5.0 | Pass | a send to a stopped computer: receipt `queued`, `<computer> is offline`, no hang. Stopping the service on a computer with open agent sessions is not an offline test: their `lightbulb mcp` restarts the daemon |
| R2 | Linux `lightbulb update` checks the signed checksum | 0.6.0 | Pass | update from 0.5.0 installed; release carries `.sha256.sig` |

## Bugs found and fixed during this run

- Linux: a process's start time followed the wall clock, so after a clock step (WSL) every tool call
  answered `source_unidentified`. Births now count from boot (0.6.1).

Rows for features since removed (sign-in, workspace agents, hosted apps) were dropped from this record.
