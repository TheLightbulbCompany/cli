# CLI reference

Every command, with the same options and arguments its `--help` lists. Each
`Usage:` line and `--help` line is the binary's own text; the rest says the same in plain words.
Run `lightbulb <command> --help` for the full text in your terminal.

Commands that change who is in the team or how it connects (`held`, `remove`, `relay <url>`,
`leave`) must be run by a person at a terminal, not by an agent. Run from an agent, they answer
`Run this from your own terminal, not from an agent.` An agent may run `invite` (always
one-time; its person is told when it is used, with a Remove button), `join`, `approve` and
`decline`.

## Getting started

### setup

When you use it: once after installing, after an upgrade, or to turn Lightbulb off.

```
lightbulb setup [OPTIONS]
```

`--help`: Wire the installed agents (Claude Code, Codex, Cursor, Grok CLI, OpenCode) to this engine: their hooks and MCP server entry. Each file is backed up once as `<file>.lightbulb-backup`.

Connects Claude Code, Codex, Cursor, Grok CLI and OpenCode here (their hooks and the `lightbulb` tool
server) and starts the background service (on macOS; off macOS it restarts a daemon that runs an older version). Each file is backed up once as
`<file>.lightbulb-backup`. Details: [agents](agents.md).

Setup then downloads the memory model (about 31 MB, once, into the state folder) and prints
`Memory model installed (NN MB)`. If the download fails or takes too long, it prints
`Memory model not installed: <reason> Memory search is keyword-only.` and carries on.

| Option | Meaning |
|---|---|
| `--remove` | Take out only what setup wrote, and stop the background service. Backups are left as they are. |
| `--executable <path>` | The binary the hooks, tool servers and background service run. Default: `~/.lightbulb/connect/bin/lightbulb`, which setup installs from the binary you ran. |
| `--server-name <name>` | The tool server's name, also used in Cursor's and Grok's allow rules. Default: `lightbulb`. |
| `--verbose` | Also list the files each agent's setup changed. |
| `--no-model` | Do not download the memory model. Memory search is keyword-only. The choice is kept (`memory_model` in `config.toml`), also through the automatic setup after an update, until you run `lightbulb setup` again without it. |

### doctor

When you use it: when something does not work.

```
lightbulb doctor
```

`--help`: Check the daemon, state permissions, agent wiring, alias and launchd job.

Checks the background service, the relay, the local network (macOS only), the login item (macOS
only), the state folder, each agent, the `lightbulb` command, the memory model, auto-update and usage counts, and
prints the fix for the first thing that is wrong.
Exits 1 when something is wrong. Every line: [troubleshooting](troubleshooting.md).

### invite

When you use it: to add your other computer or a teammate.

```
lightbulb invite [OPTIONS]
```

`--help`: Print this computer's join line (the reusable invite, unless a flag says otherwise).

Prints a four-word code (it works once, for 10 minutes), a `https://thelightbulb.company/j#…`
link and this computer's join line (the reusable invite unless an option says otherwise; reusable
for 7 days). After `--new` it says `Every earlier invite from this computer stopped working.`

| Option | Meaning |
|---|---|
| `--new` | A new invite; every earlier one from this computer stops working. |
| `--once` | An invite for one join, valid 24 hours. |
| `--agents` | Add your Lightbulb workspace's agents to this network. You must be a workspace admin, signed in here with `lightbulb login`: confirm in the browser, and the workspace joins. |

### join

When you use it: on the new computer, with the line someone sent you.

```
lightbulb join [OPTIONS] <TICKET>
```

`--help`: Join the network a join line is for (this computer must be on its own).

Joins the team the line is for. This computer must be on its own.

| Argument / option | Meaning |
|---|---|
| `<TICKET>` | What your inviter sent: the four-word code, the `https://thelightbulb.company/j#…` link, the `lightbulb://join/…` link or the long ticket from the join line. |
| `--name <name>` | The name you go by in the team. Default: your user name. Needed only if the name is taken (`The name "sam" is already used in this network.`): the message names one to try. |

## Everyday

### who

When you use it: to see which sessions are open, and how each gets messages.

```
lightbulb who [OPTIONS]
```

`--help`: List the coding sessions on this computer (with `--all`, then every teammate's).

Lists the coding sessions on this computer: name (such as `codex-1`), agent, folder, status and
what it is doing.

| Option | Meaning |
|---|---|
| `--all` | Also list the sessions on teammates' computers (with person and computer). |
| `--json` | Print the raw session list as JSON. |

### send

When you use it: to message a session from your terminal.

```
lightbulb send [OPTIONS] <TARGET> <TEXT>...
```

`--help`: Send text to a session (`session_id`), a name address (`<member>/<name>`), or, from a person's terminal, a group (`*`, `<member>/*`).

`<TARGET>` is a name from `who` (`codex-1`), a session id, a name address `<member>/<name>`
(for example `sam/inbox`, where `sam` is a teammate's name), `*` for every open session on
this computer, or `<member>/*` (for example `'sam/*'`) for each of that teammate's sessions as
`who --all` lists them. The rest of the line is the text. It prints the receipt, for example
`Sent. Check: lightbulb receipt <id>`.

| Option | Meaning |
|---|---|
| `--from <session>` | Send as this session (a person speaking for it) instead of the terminal. |
| `--reply-to <id>` | The request id of the message this answers. |
| `--request-id <id>` | Use this request id instead of a new one (to retry a send). |

Group sends (`*`, `<member>/*`) are for people at a terminal only.

### listen

When you use it: to read messages sent to a name, for example from a script.

```
lightbulb listen [OPTIONS] --as <NAME>
```

`--help`: Print messages addressed to the name `<member>/<name>` as they arrive (Ctrl-C to stop).

Prints messages addressed to `<your member>/<name>` as they arrive, each with a `Reply:` line.
Ctrl-C stops it. `<name>` takes letters, digits, `.`, `_` and `-` (1 to 64).

| Option | Meaning |
|---|---|
| `--as <name>` | The name to listen on. |
| `--once` | Return after the first message. |

### receipt

When you use it: to see whether a message arrived.

```
lightbulb receipt [OPTIONS] <REQUEST_ID>
```

`--help`: Print the receipt of a message sent from this terminal (or `--from` another address).

Prints the receipt of a message sent from this terminal. States: `queued`, `stored`,
`delivered`, `read`, `refused`, `expired`.

| Option | Meaning |
|---|---|
| `--from <address>` | A message sent from another address (a session) instead of this terminal. |
| `--json` | Print the receipt as JSON. |

### held

When you use it: to see the permission prompts the Lightbulb app filed for you to answer.

```
lightbulb held
```

`--help`: The permission prompts waiting for your answer.

Lists them as `H<n>`, each with the command that answers it. Joins and messages never wait
here: an invite is the approval, and everyone on the network is trusted.

### approve

When you use it: to allow a permission prompt.

```
lightbulb approve <REQUEST>
```

`--help`: Allow a permission prompt (`H<n>`).

### decline

When you use it: to refuse a permission prompt.

```
lightbulb decline <REQUEST>
```

`--help`: Decline a permission prompt (`H<n>`).

## Team

### team

When you use it: to see the team, its computers, their roles, and who is offline.

```
lightbulb team
```

`--help`: Show the team and its members.

### remove

When you use it: to take a computer out of the team.

```
lightbulb remove <NAME>
```

`--help`: Remove a member; the invite is replaced.

Any member can remove another. Every live invite in the team stops working; the answer has a new invite
line.

### leave

When you use it: to take this computer out of its team.

```
lightbulb leave
```

`--help`: Leave the team; this computer is on its own network again.

Asks `[y/N]` first. This computer is on its own network again. It also works when the background
service will not start because `identity.key` no longer matches the team record.

### login

When you use it: to use your Lightbulb workspace's agents from this computer.

```
lightbulb login
```

Prints a code and opens `thelightbulb.company` in your browser; sign in (or make a free account)
and type the code there. This computer is then yours in Lightbulb: it keeps a sign-in proof
(`login.json`, renewed before its 30 days end), your workspaces' agents answer it, and while it
runs it keeps one connection to Lightbulb for your hosted apps ([apps](connect/apps.md)). The code
works for 10 minutes.

### logout

```
lightbulb logout
```

Deletes this computer's sign-in proof and drops its connection for hosted apps; workspaces stop
answering it on their next connection to it.

### folder

When you use it: to hide a project's sessions from your teammates.

```
lightbulb folder <PATH> <STATE>
```

`--help`: Show (`on`) or hide (`off`) the sessions under a folder from your teammates.

`off` hides the sessions under `<PATH>` from teammates; `on` shows them again.

### relay

When you use it: to see or change the relays.

```
lightbulb relay [OPTIONS] [URL]
```

`--help`: The relays this computer uses (with a URL, sets yours and the team's too). Relays carry encrypted traffic only, when no direct path exists.

With no arguments, shows the relays in use and where they come from. With a URL (`https://`
only), sets yours and the team's too. Relays carry encrypted traffic
only, when no direct path exists.

| Option | Meaning |
|---|---|
| `--clear` | Clear the relay you set. |
| `--local` | Only this computer, not the team (otherwise the team's too). |

### update

When you use it: to move to the latest release (or a chosen one) without the install line.

```
lightbulb update [OPTIONS]
```

`--help`: Install the latest release (or `--version`) over this one, then run its `setup`

Downloads the release, checks its checksum, replaces the installed binary and runs the new
one's `setup`, so the hooks, the tool server and the background service move with it. Already
on that version: it says so and changes nothing. Versions before 0.2.2 cannot be installed.
Homebrew and npm users upgrade with their package manager instead (see troubleshooting if the
version does not change afterwards).

| Option | Meaning |
|---|---|
| `--version <VERSION>` | The release to install, like `v0.2.2` (an example; default: the latest). |
| `--auto <on\|off>` | Turn the daily automatic update on or off, and install nothing. It prints `Automatic updates on.` or `Automatic updates off.`; it cannot be used with `--version`. `doctor` shows `Auto-update`. |

### version

```
lightbulb version
lightbulb --version
```

`--help`: Print the version.

Prints the version, for example `lightbulb 0.6.4`.

### telemetry

When you use it: to see or change anonymous usage counts.

```
lightbulb telemetry [on|off|status]
```

`--help`: Anonymous usage counts: `on`, `off`, or (no argument) their state.

Prints `Usage counts  on` or `Usage counts  off (…reason…)`. `off` and `on` save the choice in
`config.toml`. `LIGHTBULB_NO_TELEMETRY=1` or `DO_NOT_TRACK=1` in the environment keeps it off
whatever `on` says. What is counted: [README](../README.md#usage-counts).

### memory

When you use it: to see, share or delete what your agents have remembered, on your computer or your team's.

```
lightbulb memory list [--team | --me] [--repo <ORIGIN>]
lightbulb memory show <ID>
lightbulb memory forget <ID>
lightbulb memory share <ID>
lightbulb memory export [--json]
lightbulb memory last
lightbulb memory team <on|off>
lightbulb memory drafts
lightbulb memory approve <ID> [--team]
lightbulb memory dismiss <ID>
```

`--help`: What your agents and your team remember: list, show, forget, share, export, and the lessons suggested for you: drafts, approve, dismiss.

Agents write memory with the `remember` tool and read it with `recall`. These commands are your
own view of it. An `<ID>` is the first characters of an entry's id as `list` prints it (six or
more). `forget`, `share`, `team`, `drafts`, `approve` and `dismiss` must be run from your own terminal; an agent's shell is refused.

| Command | What it does |
|---|---|
| `list` | What is remembered now: the id, whether it is `team` or `me`, who wrote it, its status (`current`, or `contested` when two people hold different values for one subject), the subject and the value. `--team` or `--me` keeps one scope; `--repo` keeps entries made in one repository: `owner/name`, or a bare name, which matches it under any owner. |
| `show <ID>` | One entry in full: its value, its author, the agent and repository it came from, and the values it replaced. |
| `forget <ID>` | Deletes the entry. The delete goes to every computer that holds it, as each comes online. |
| `share <ID>` | Copies one of your `me` entries to the team as a new `team` entry. The `me` entry stays. It refuses an entry that is not your own `me` entry, one that was replaced, one you already shared, and any entry when `memory team` is `off`. |
| `export` | Every entry this computer holds, as stored: one JSON object per line, or one array with `--json`. A forgotten entry has no text left to export. |
| `last` | The memory block the last session was handed at its start, word for word. Any `forget` clears it (`No session has been handed a memory block yet.`) until the next session starts. |
| `team on\|off` | Whether team memory may be written from this computer (`memory_team` in `config.toml`, on by default). `off` makes `remember` and `share` refuse `team` entries; what is already held stays. |
| `drafts` | The lessons Lightbulb's evaluator suggests from sessions you uploaded in full (see `sessions upload`): the id, the kind, the first line and how many sessions it came from. Needs `lightbulb login`. |
| `approve <ID> [--team]` | Keeps a suggested lesson. Signed in on your own, it becomes a `me` entry on this computer (a `team` entry with `--team`), marked as from the cloud evaluator. In a workspace it says `Approved; the workspace will remember it` and the entry arrives through the network; `--team` shares it with the whole workspace. It says so if the draft was settled elsewhere, or refused (a secret, or too long). |
| `dismiss <ID>` | Throws a suggested lesson away. |

### sessions

When you use it: to see, read, export or delete what Lightbulb captured of your coding sessions on this computer.

```
lightbulb sessions list
lightbulb sessions show <ID>
lightbulb sessions forget <ID>
lightbulb sessions export
lightbulb sessions upload [off|metadata|full]
lightbulb sessions turns [on|off]
```

`--help`: The sessions captured on this computer: list, show, forget, export, and what is uploaded.

Each agent's hooks tell the background service when a session starts, after each tool call, at
the end of each turn and when the session ends. It keeps, on this computer only: the session (its
id, the agent, the computer, the repository and branch, when it started and ended, and the
tokens, for Claude Code only, summed from its transcript once, when the session ends; the other agents leave them blank), and for each tool call its number, time, tool, duration,
whether it worked, and a summary of at most 200 characters (a command, a path, a pattern). It
never keeps file contents, diffs or command output. It also records your prompts and the
agent's replies on your computer only (see `turns`). A summary, prompt or reply that looks like a
password or key is replaced by `[refused: <what it matched>]`. Sessions under a hidden folder
(`lightbulb folder <path> off`, or a folder in your home folder whose name starts with a dot,
such as `~/.config`) are not captured; a dot folder inside a repository, such as Claude Code's
`.claude/worktrees`, is. Capture never slows the agent: a write the background service has not
answered within 10 ms (250 ms for the end-of-turn answer that decides the nudge) is not waited for (the service still makes it). An
`<ID>` is the id as `list` prints it, or its first characters. `list`, `show`, `forget` and
`export` must be run from your own terminal; an agent's shell is refused.

| Command | What it does |
|---|---|
| `list` | The id, the agent, the repository, the day it started and the tokens in and out (`-` where the agent did not report them). |
| `show <ID>` | One session, its tool calls in order, then its prompts and replies in order. |
| `forget <ID>` | Deletes the session, its tool calls and its prompts and replies. |
| `export` | Every session with its tool calls, as one JSON array on stdout. Prompts and replies are left out. |
| `upload [off\|metadata\|full]` | Choose what, if anything, is sent to Lightbulb (`capture_upload` in `config.toml`). `off` is the default: nothing leaves this computer. (The one request memory makes is setup's download of the memory model; `setup --no-model` skips it.) `metadata` sends, for each finished session, the agent, repository, branch, times and token counts, and for each tool call its number, time, tool, duration and whether it worked, without the summary. `full` adds the summary. Uploads are signed with this computer's identity, only while it is signed in, only for sessions that finish after you turn them on, and never for a hidden folder; signed out, it says nothing is sent until `lightbulb login`. Setting a mode must be run from your own terminal (an agent's shell is refused); with no mode it says which is set, from anywhere. `lightbulb login` asks once. |
| `turns [on\|off]` | Records your prompts and replies on your computer only; never uploaded, in any mode (`capture_turns` in `config.toml`, on by default). Each is cut to 4 KB and deleted after 90 days (`capture_turns_days`). `sessions turns off` stops it, and `recall` stops using the turns already stored (they stay until you delete them with `sessions forget <ID>`); setting it must be run from your own terminal (an agent's shell is refused); with no state it says which is set. |

## Apps you build

### app

When you use it: to build a small web app for your workspace, check it, preview it, publish it and undo it. Not the hosted assistants; see [apps you build](apps.md).

```
lightbulb app new <NAME> [--room <ROOM>] [--kind <KIND>]
lightbulb app up [--room <ROOM>] [--approved <CARD>]
lightbulb app publish [--reason <TEXT>] [--room <ROOM>] [--approved <CARD>]
lightbulb app rollback [VERSION]
lightbulb app status [--errors]
lightbulb app destroy
lightbulb app check [--publish]
lightbulb app evidence --kind <browser|review> --file <FILE> [--note <TEXT>] [--verdict <pass|fail>]
lightbulb app report-broken-tool <TOOL> <WHAT>
```

`--help`: Build, check, preview, publish and undo apps. Every subcommand takes `--json` (the default when stdout is not a terminal); a failure prints `{"code","message","next"}` and exits 1.

| Command | What it does |
|---|---|
| `new` | Creates `./<NAME>` from the starter with one git commit; next, `pnpm install` in it. `--kind` is `site` (default), `content-site`, `form`, `internal-tool`, `dashboard`, `portal`, `payments` or `automation`. `--room` is saved in `.lightbulb/app.json`. |
| `up` | Sends HEAD, waits for the preview build and reports `done` or what is `missing`. `--approved` is the approved card for a destructive migration. A failed build exits 1 with `{"code","message","next"}`. |
| `publish` | Puts the preview live as a new version. Needs `check --publish` to pass and a passing review for HEAD. |
| `rollback` | Makes an earlier version live (default: the one before) and adds one commit with its files. Code only; data is unchanged (the `error.code: "code_only"` it returns says so; it is not a failure). Refuses with uncommitted changes. |
| `status` | State, URLs, versions and the done gate. `--errors` adds the last client and server errors. |
| `destroy` | Asks for removal and prints the backend's state (`awaiting_owner` until the owner removes it in Canvas). |
| `check` | Type check, lint and design lint (`pnpm check`), errors parsed. `--publish` adds the dependency audit (`pnpm check:publish`). Fails with `not_installed` before `pnpm install`. |
| `evidence` | Records a browser pass or a review for HEAD. A review needs `--verdict`. The file stays on this computer. |
| `report-broken-tool` | Notes a failing tool in `.lightbulb/lessons.md`, once per `up`. |

Run all but `new` inside the app's folder. Auth is a workspace agent's hook token when
`OPENCLAW_HOOK_TOKEN`, `ISOL8_API_URL` and `ISOL8_OWNER_ID` are all set (`ISOL8_API_URL` is the
API origin; the CLI appends `/api/v1` once); otherwise `lightbulb login`.
Details: [apps you build](apps.md).

## Commands the agents and macOS run

`hook`, `mcp` and `daemon` are not for typing. The agents run `lightbulb hook <agent> <event>`
and `lightbulb mcp`; macOS's login service runs `lightbulb daemon` (off macOS, run it yourself, or let the CLI start it). `setup` wires them.
