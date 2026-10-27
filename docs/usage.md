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
  `~/Library/LaunchAgents/company.thelightbulb.connect.plist`. It starts at login. Off macOS there
  is no such job: the CLI starts the daemon when a command needs it, or you run `lightbulb daemon`
  yourself (keep it running, for example in tmux or a systemd `--user` unit). `update` and `setup`
  restart a daemon that runs an older version.
- **Agent wiring:** for Claude Code, Codex, Cursor, Grok CLI and OpenCode, `setup` adds hooks (session start,
  prompt, tool use, stop, session end; for OpenCode a plugin that runs them) and one tool server,
  in each agent's own config files (`~/.claude/settings.json`, `~/.claude.json`, `~/.codex/`,
  `~/.cursor/`, `~/.grok/`, `~/.config/opencode/`). Each file
  is backed up once as `<file>.lightbulb-backup`. The tool server is named `lightbulb`; `setup` replaces an
  existing `lightbulb` entry only when it is ours, the desktop app's or the hosted MCP's. Run `setup --verbose` to
  list the files it changed.

It does not change your projects, your repositories or any other entry in those config files.
It does not read the files in your project folders.

## Commands

Run `lightbulb <command> --help` for the options of each one.

| Command | What it does |
|---|---|
| `setup` | Connect Claude Code, Codex, Cursor, Grok CLI and OpenCode here and start the background service. |
| `update` | Install the latest release over this one and restart the background service. `--version v0.2.3` installs that release instead (0.2.2 or later; an example tag). `--auto on\|off` turns the daily automatic update on or off and installs nothing. |
| `setup --remove` | Take out only what `setup` wrote. Backups are left as they are. |
| `doctor` | Check the service, the local network, the state folder and each agent, and say how to fix what is not right. |
| `who` | List the sessions on this computer. `who --all` adds your teammates' sessions. |
| `invite` | Print a four-word code (works once, for 10 minutes), a `https://thelightbulb.company/j#…` link, and the join command (`lightbulb join '<ticket>'`) that add your other computer or a teammate. The link and line are reusable for 7 days. `--once`: one join, valid 24 hours. `--new`: a new invite; every earlier one from this computer, `--once` ones too, stops working. `--agents`: add your Lightbulb workspace's agents to this network (a workspace admin, signed in; confirm in the browser). |
| `join '<ticket>'` | Join the team the invite is for. Takes the four-word code, the `https://thelightbulb.company/j#…` link, the `lightbulb://join/…` link or the bare ticket. The name you go by is your user name; `--name <name>` sets another. The computer must be on its own first. |
| `approve H1` / `decline H1` | Answer a permission prompt the Lightbulb app filed. |
| `held` | The permission prompts waiting for your answer. |
| `send <to> "<text>"` | Send a message from the terminal: `send codex-1 "hi"`, or to a teammate's name: `send sam/inbox "hi"`. From your own terminal, `send 'sam/*' "hi"` sends to each of sam's sessions (as `who --all` lists them), and `send '*' "hi"` to every session on this computer. |
| `listen --as <name>` | Print the messages sent to a name on this computer. |
| `receipt <id>` | Whether a message you sent arrived. `--json` for the raw receipt. |
| `team` | The team and its computers. |
| `remove <name>` | Remove a member. The invite is replaced. |
| `leave` | Leave the team. This computer is on its own network again. It also works when the background service will not start because `identity.key` no longer matches the team record. |
| `relay [<url>]` | Show the relays this computer uses, or set one. With nothing set it uses the iroh project's public relays (`default`). It sets the team's relay; `--local` sets only this computer's; `--clear` clears yours and goes back to the default. Relays carry encrypted bytes only. Lightbulb fetches no relay list of its own. |
| `folder <path> on\|off` | Show or hide the sessions under a folder from your teammates. |
| `login` | Sign this computer in to Lightbulb (confirm in the browser), so your workspace's agents answer it. |
| `logout` | Sign this computer out. |
| `telemetry [on\|off\|status]` | Show or set anonymous usage counts (see the [README](../README.md#usage-counts)). |
| `version` | Print the version. |

`held`, `remove`, `leave` and `relay <url>` refuse to run from an agent's shell
(`Run this from your own terminal, not from an agent.`). An agent may run `invite`, `join`,
`approve` and `decline`; an answer it gives records its session.

## Trust

Everyone on the network is trusted: a message from a member goes straight to the session it
names, and an invite is the approval for a join. Nothing is held and nothing asks you. To stop
someone, `lightbulb remove <name>`. When an agent's invite brings someone in, a dialog tells you
(Keep or Remove).

## What teammates can see

For each session on your computer, teammates see:

- the agent (Claude Code, Codex, Cursor, Grok CLI, OpenCode) and the computer it runs on;
- the folder's name (its last part, not the full path), and the repository and branch;
- whether it is busy or idle, and a short title of what it is doing.

They never see your messages, your files or your agents' conversations. Messages travel end to end
encrypted between the two computers, and their text is not written to the log. A message's
text is deleted from `state.db` once nothing waits on it (read, refused or expired); messages to
your own terminal keep theirs.

Apart from anonymous usage counts (`lightbulb telemetry off`), the engine calls Lightbulb only for
`login`, renewing a signed-in computer's proof, `invite --agents`, and, while signed in, the live
connection hosted apps use. Otherwise it talks only to your teammates' computers and, when no
direct path exists, to the relays. The background service checks this project's GitHub releases
once a day for an update (`lightbulb update --auto off` stops it), and `update` downloads from them.

To hide sessions, run `lightbulb folder <path> off`. Sessions under that folder disappear
from your teammates' `who --all` within a few seconds. `who` on your
own computer marks them `hidden from teammates`. `folder <path> on` shows them again.

## When a computer is offline

Nothing is lost when a Mac is asleep or offline. A message to it waits on the sender's computer,
and its receipt reads `queued`:
`Not delivered yet: <computer> is offline. Lightbulb keeps trying until <time>.`
The sender keeps trying for 24 hours. When the other Mac comes back, the message arrives (stored on arrival). After 24 hours an undelivered message expires.

A message that reaches the other Mac but waits there unread for 24 hours (the session never came
back) expires too. The sender's receipt then reads
`<session> did not come back within 24 hours; the message expired.`

If the service will not start and says
`identity.key does not match this computer's team record. Run lightbulb leave, then join again with a new invite.`,
the key was replaced while the messages were kept. Do what it says: `leave` works without the
service, then ask a teammate for a new invite.

`join` needs the inviter's Mac to be online. If it is not, `join` says so; run the same command
again later.

## Uninstall

```sh
lightbulb setup --remove
```

It takes out the hooks and tool servers it added to each agent, removes the launchd job (on macOS) and stops
the service. It prints `Lightbulb is off. Your settings and messages are kept in
~/.lightbulb/connect.` To erase this computer's key and messages too, delete that folder.
Restart any open agent sessions so they drop the tool server.

## Reporting problems

Open an issue on the repository's
[issues page](https://github.com/TheLightbulbCompany/cli/issues). Include the output
of `lightbulb doctor` and `lightbulb version`. Do not paste message text or
`identity.key`.

Report security problems privately, as [SECURITY.md](../SECURITY.md) describes, not in an issue.
