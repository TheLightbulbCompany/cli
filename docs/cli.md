# CLI reference

Every command, with the same options and arguments its `--help` lists (version 0.3.7). Each
`Usage:` line and `--help` line is the binary's own text; the rest says the same in plain words.
Run `lightbulb <command> --help` for the full text in your terminal.

Commands that change who is in the team or how it connects (`held`, `policy <person> <value>`,
`remove`, `relay <url>`, `leave`) must be run by a person at a terminal, not by an agent. Run
from an agent, they answer `Run this from your own terminal, not from an agent.` An agent may
run `invite` (every mode), `join`, `approve` and `decline`; its session is recorded as the
approver.

## Getting started

### setup

When you use it: once after installing, after an upgrade, or to turn Lightbulb off.

```
lightbulb setup [OPTIONS]
```

`--help`: Wire the installed agents (Claude Code, Codex, Cursor, Grok CLI) to this engine: their hooks and MCP server entry. Each file is backed up once as `<file>.lightbulb-backup`.

Connects Claude Code, Codex, Cursor and Grok CLI here (their hooks and the `lightbulb-network` tool
server) and starts the background service (on macOS; off macOS it restarts a daemon that runs an older version). Each file is backed up once as
`<file>.lightbulb-backup`. Details: [agents](agents.md).

| Option | Meaning |
|---|---|
| `--remove` | Take out only what setup wrote, and stop the background service. Backups are left as they are. |
| `--executable <path>` | The binary the hooks, tool servers and background service run. Default: `~/.lightbulb/connect/bin/lightbulb`, which setup installs from the binary you ran. |
| `--server-name <name>` | The tool server's name, also used in Cursor's and Grok's allow rules. Default: `lightbulb-network`. |
| `--verbose` | Also list the files each agent's setup changed. |

### doctor

When you use it: when something does not work.

```
lightbulb doctor
```

`--help`: Check the daemon, state permissions, agent wiring, alias and launchd job.

Checks the background service, the relay, the local network (macOS only), the login item (macOS
only), the state folder, each agent, the `lightbulb` command, auto-update and usage counts, and
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
| `--new` | A new invite; every earlier one from this computer (open ones too) stops working. |
| `--once` | An invite for one join, valid 24 hours. |
| `--open` | An invite whose joins need no Allow |

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
`Sent. sam has to allow it first (they see a prompt). Check: lightbulb receipt <id>`.

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

Prints the receipt of a message sent from this terminal. States: `queued`, `held`, `stored`,
`delivered`, `read`, `refused`, `expired`.

| Option | Meaning |
|---|---|
| `--from <address>` | A message sent from another address (a session) instead of this terminal. |
| `--json` | Print the receipt as JSON. |

### held

When you use it: to see what waits for your Allow.

```
lightbulb held
```

`--help`: The join requests and held messages waiting for your Allow.

Lists join requests (`J<n>`) and held messages (`H<n>`), each with the command that allows it.

### approve

When you use it: to allow a join request or a held message.

```
lightbulb approve [OPTIONS] <REQUEST>
```

`--help`: Allow a join request (`J<n>`) or a held message (`H<n>`).

`<REQUEST>` is `J<n>` or `H<n>` from `held`. Allowing a held message from a teammate's computer
also lets that person's later messages through.

| Option | Meaning |
|---|---|
| `--always` | Only for messages from a cloud member's agents. It is not used between computers; on any other message it answers `--always is only for messages from a cloud member's agents.` |

### decline

When you use it: to refuse a join request or a held message.

```
lightbulb decline <REQUEST>
```

`--help`: Decline a join request (`J<n>`) or a held message (`H<n>`).

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

### policy

When you use it: to choose who may message your sessions without asking.

```
lightbulb policy [PERSON] [VALUE]
```

`--help`: Who may message your sessions; with a person and a value, sets it.

With no arguments, shows the policy. `<PERSON>` is a teammate's name or short address, as
`team` shows it.

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

Prints the version, for example `lightbulb 0.3.7`.

### telemetry

When you use it: to see or change anonymous usage counts.

```
lightbulb telemetry [on|off|status]
```

`--help`: Anonymous usage counts: `on`, `off`, or (no argument) their state.

Prints `Usage counts  on` or `Usage counts  off (…reason…)`. `off` and `on` save the choice in
`config.toml`. `LIGHTBULB_NO_TELEMETRY=1` or `DO_NOT_TRACK=1` in the environment keeps it off
whatever `on` says. What is counted: [README](../README.md#usage-counts).

## Commands the agents and macOS run

`hook`, `mcp` and `daemon` are not for typing. The agents run `lightbulb hook <agent> <event>`
and `lightbulb mcp`; macOS's login service runs `lightbulb daemon` (off macOS, run it yourself, or let the CLI start it). `setup` wires them.
