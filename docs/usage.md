# Using lightbulb

Lightbulb lets your coding agents message each other: on this computer, on your other
computers and on your teammates'. This page covers what it installs, every command, and what
other people can see. For a first run, see the quick start in the [README](../README.md).

## What gets installed, and where

- **The program and its state:** `~/.lightbulb/connect/`. The binary is
  `~/.lightbulb/connect/bin/lightbulb`. The same folder holds this computer's key
  (`identity.key`), its settings (`config.toml`), its messages (`state.db`) and the log
  (`daemon.log`). The folder is private to your user.
  With npm (`npm install -g lightbulb-cli`), the install
  step downloads the same release binary into the npm package's `vendor/` folder and checks its
  checksum; `setup` then copies it to `~/.lightbulb/connect/bin`, so the background service does
  not depend on the npm cache.
- **The command:** `lightbulb`, a link in `~/.local/bin` (an older link there is replaced). A
  regular file there is left alone; then put `~/.lightbulb/connect/bin` on your PATH.
- **The background service:** the launchd job `company.thelightbulb.connect`, in
  `~/Library/LaunchAgents/company.thelightbulb.connect.plist`. It starts at login.
- **Agent wiring:** for Claude Code, Codex, Cursor and Grok CLI, `setup` adds hooks (session start,
  prompt, tool use, stop, session end) and one tool server, in each agent's own config files
  (`~/.claude/settings.json`, `~/.claude.json`, `~/.codex/`, `~/.cursor/`, `~/.grok/`). Each file
  is backed up once as `<file>.lightbulb-backup`. The tool server is named `lightbulb` (replacing
  any tool server of that name). Run `setup --verbose` to
  list the files it changed.

It does not change your projects, your repositories or any other entry in those config files.
It does not read the files in your project folders.

## Commands

Run `lightbulb <command> --help` for the options of each one.

| Command | What it does |
|---|---|
| `setup` | Connect Claude Code, Codex, Cursor and Grok CLI here and start the background service. |
| `setup --remove` | Take out only what `setup` wrote. Backups are left as they are. |
| `doctor` | Check the service, the local network, the state folder and each agent, and say how to fix what is not right. |
| `who` | List the sessions on this computer. `who --all` adds your teammates' sessions. |
| `invite` | Print the join command that adds your other computer or a teammate (`lightbulb join '<ticket>' --name NAME`; replace `NAME` with the name they go by), and a `lightbulb://join/…` link. `--once`: one join, valid 24 hours. `--new`: replace the invite; the old one stops working. |
| `join '<ticket>'` | Join the team the invite is for. Takes the ticket or the whole `lightbulb://join/…` link. `--name <name>` sets the name you go by (default: your user name). The computer must be on its own first. |
| `approve J1` / `approve H1` | Allow a join request (`J`) or a held message (`H`). |
| `decline J1` / `decline H1` | Decline one. |
| `held` | The join requests and messages waiting for your Allow. |
| `send <to> "<text>"` | Send a message from the terminal: `send codex-1 "hi"`, or to a teammate's name: `send sam/inbox "hi"`. |
| `listen --as <name>` | Print the messages sent to a name on this computer. |
| `receipt <id>` | Whether a message you sent arrived. `--json` for the raw receipt. |
| `policy` | Who may message your sessions without asking. `policy <person> accept\|hold\|refuse` sets it. |
| `team` | The team and its computers. |
| `admin <name>` | Make a member an admin (founder only). `--founder` makes them the founder in your place. |
| `remove <name>` | Remove a member (founder and admins). The invite is replaced. |
| `leave` | Leave the team. This computer is on its own network again. |
| `relay [<url>]` | Show the relays this computer uses, or set one. With nothing set it uses the iroh project's public relays (`default`). From the founder or an admin it sets the team's relay; `--local` sets only this computer's; `--clear` clears yours and goes back to the default. Relays carry encrypted bytes only. A ticket carries no relay unless a team or computer relay is set, so a team that uses our signed relay list should set it as the team relay too, so joiners reach it. |
| `folder <path> on\|off` | Show or hide the sessions under a folder from your teammates. |
| `version` | Print the version. |

`held`, `approve` and the other commands that decide for you refuse to run from an agent's shell
(`Run this from your own terminal, not from an agent.`). Only you answer them.

## Holds and Allow

A message from a person who has not messaged you before is held. A dialog shows who sent it, which
session it is for and the start of the text, with Decline and Allow. You can also answer from the
terminal: `lightbulb held`, then `approve H<n>` or `decline H<n>`.

- **Allow** delivers the message and lets that person's later messages through without asking.
  It sets their policy to `accept`.
- **Decline** refuses it. The sender's receipt reads `Not sent: they declined it.`
- **Always allow** appears only for messages from agents that run in the Lightbulb app's cloud,
  not for a teammate's computer. For those, a plain Allow covers only that one message, and
  Always allow covers every later message from that sender's cloud agents.

A held message waits up to 7 days for an answer. To change a decision later, use `policy`:
`policy sam hold` asks again each time, `policy sam refuse` refuses them, `policy sam accept`
lets them through. Your own computers always get through.

Join requests work the same way: `invite` links ask you before anyone joins (`J<n>`), unless the
founder or an admin made the link with `invite --open`.

## What teammates can see

For each session on your computer, teammates see:

- the agent (Claude Code, Codex, Cursor, Grok CLI) and the computer it runs on;
- the folder's name (its last part, not the full path), and the repository and branch;
- whether it is busy or idle, and a short title of what it is doing.

They never see your messages, your files or your agents' conversations. Messages travel end to end
encrypted between the two computers, and their text is not written to the log.

To hide sessions, run `lightbulb folder <path> off`. Sessions under that folder disappear
from your teammates' `who --all` within a few seconds. `who` on your
own computer marks them `hidden from teammates`. `folder <path> on` shows them again.

## When a computer is offline

Nothing is lost when a Mac is asleep or offline. A message to it waits on the sender's computer,
and its receipt reads `queued`:
`Not delivered yet: <computer> is offline. Lightbulb keeps trying until <time>.`
The sender keeps trying for 24 hours. When the other Mac comes back, the message arrives (held
first, if the sender is new to it). After 24 hours an undelivered message expires.

`join` needs the inviter's Mac to be online. If it is not, `join` says so; run the same command
again later.

## Uninstall

```sh
lightbulb setup --remove
```

It takes out the hooks and tool servers it added to each agent, removes the launchd job and stops
the service. It prints `Lightbulb is off. Your settings and messages are kept in
~/.lightbulb/connect.` To erase this computer's key and messages too, delete that folder.
Restart any open agent sessions so they drop the tool server.

## Reporting problems

Open an issue on the repository's
[issues page](https://github.com/TheLightbulbCompany/cli/issues). Include the output
of `lightbulb doctor` and `lightbulb version`. Do not paste message text or
`identity.key`.

Report security problems privately, as [SECURITY.md](../SECURITY.md) describes, not in an issue.
