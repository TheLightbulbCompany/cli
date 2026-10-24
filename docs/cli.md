# CLI reference

Every command, with the options its `--help` lists (version 0.2.2). Run
`lightbulb <command> --help` for the same text in your terminal.

Commands that decide for you (`held`, `approve`, `decline`, `policy <person> <value>`, `remove`,
`admin`, `invite --open`, `relay <url>`, `signin`, `leave`) must be run by a person at a
terminal, not by an agent.

## Getting started

### setup

When you use it: once after installing, after an upgrade, or to turn Lightbulb off.

```
lightbulb setup [--remove] [--executable <path>] [--server-name <name>] [--verbose]
```

Connects Claude Code, Codex, Cursor and Grok CLI here (their hooks and the `lightbulb` tool
server) and starts the background service. Each file is backed up once as
`<file>.lightbulb-backup`. Details: [agents](agents.md).

| Option | Meaning |
|---|---|
| `--remove` | Take out only what setup wrote, and stop the background service. Backups are left as they are. |
| `--executable <path>` | The binary the hooks, tool servers and background service run. Default: `~/.lightbulb/connect/bin/lightbulb`, which setup installs from the binary you ran. |
| `--server-name <name>` | The tool server's name, also used in Cursor's and Grok's allow rules. Default: `lightbulb`. |
| `--verbose` | Also list the files each agent's setup changed. |

### doctor

When you use it: when something does not work.

```
lightbulb doctor
```

Checks the background service, the relay, the local network, the login item, the state folder,
each agent and the `lightbulb` command, and prints the fix for the first thing that is wrong.
Exits 1 when something is wrong. Every line: [troubleshooting](troubleshooting.md).

### invite

When you use it: to add your other computer or a teammate.

```
lightbulb invite [--new | --once | --open]
```

Prints this computer's join line (the reusable invite unless an option says otherwise).

| Option | Meaning |
|---|---|
| `--new` | Replace the reusable invite; the old one stops working. |
| `--once` | An invite for one join, valid 24 hours. |
| `--open` | An invite whose joins need no Allow (founder and admins). |

### join

When you use it: on the new computer, with the line someone sent you.

```
lightbulb join [--name <name>] <TICKET>
```

Joins the team the line is for. This computer must be on its own.

| Argument / option | Meaning |
|---|---|
| `<TICKET>` | The line your inviter sent: the long code after `join`, or the whole `lightbulb://join/…` link. |
| `--name <name>` | The name you go by in the team. Default: your user name. |

### signin

When you use it: to show teammates your verified name. (In this release it cannot finish yet;
see [teams](teams.md#sign-in).)

```
lightbulb signin
```

Signs in to Lightbulb in your browser.

## Everyday

### who

When you use it: to see which sessions are open, and how each gets messages.

```
lightbulb who [--all] [--json]
```

Lists the coding sessions on this computer: name (such as `codex-1`), agent, folder, status and
what it is doing.

| Option | Meaning |
|---|---|
| `--all` | Also list the sessions on teammates' computers (with person and computer). |
| `--json` | Print the raw session list as JSON. |

### send

When you use it: to message a session from your terminal.

```
lightbulb send [--from <session>] [--reply-to <id>] [--request-id <id>] <TARGET> <TEXT>...
```

`<TARGET>` is a name from `who` (`codex-1`), a session id, a name address `<member>/<name>`
(for example `sam/inbox`, where `sam` is a teammate's name), or `*` for every open session on
this computer. The rest of the line is the text. It prints the receipt, for example
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
lightbulb listen --as <name> [--once]
```

Prints messages addressed to `<your member>/<name>` as they arrive, each with a `Reply:` line.
Ctrl-C stops it. `<name>` takes letters, digits, `.`, `_` and `-` (1 to 64).

| Option | Meaning |
|---|---|
| `--as <name>` | The name to listen on. |
| `--once` | Return after the first message. |

### receipt

When you use it: to see whether a message arrived.

```
lightbulb receipt [--from <address>] [--json] <REQUEST_ID>
```

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

Lists join requests (`J<n>`) and held messages (`H<n>`), each with the command that allows it.

### approve

When you use it: to allow a join request or a held message.

```
lightbulb approve [--always] <REQUEST>
```

`<REQUEST>` is `J<n>` or `H<n>` from `held`. Allowing a held message from a teammate's computer
also lets that person's later messages through. `--always` does not apply to messages between
computers; leave it off (it then answers `--always is only for messages from a cloud member's
agents.`).

### decline

When you use it: to refuse a join request or a held message.

```
lightbulb decline <REQUEST>
```

## Team

### team

When you use it: to see the team, its computers, their roles, and who is offline.

```
lightbulb team
```

### admin

When you use it: as the founder, to make an admin or hand over the founder role.

```
lightbulb admin [--founder] <NAME>
```

Founder only. With `--founder`, `<NAME>` becomes the founder and you become an admin.

### remove

When you use it: to take a computer out of the team.

```
lightbulb remove <NAME>
```

Founder and admins. Every live invite in the team stops working; the answer has a new invite
line.

### leave

When you use it: to take this computer out of its team.

```
lightbulb leave
```

Asks `[y/N]` first. This computer is on its own network again.

### policy

When you use it: to choose who may message your sessions without asking.

```
lightbulb policy [<PERSON> <accept|hold|refuse>]
```

With no arguments, shows the policy. `<PERSON>` is a teammate's name or short address, as
`team` shows it.

### folder

When you use it: to hide a project's sessions from your teammates.

```
lightbulb folder <PATH> <on|off>
```

`off` hides the sessions under `<PATH>` from teammates; `on` shows them again.

### relay

When you use it: to see or change the relays (not listed in `lightbulb --help`).

```
lightbulb relay [<URL>] [--clear] [--local]
```

With no arguments, shows the relays in use and where they come from. With a URL (`https://`
only), sets yours; from the founder or an admin, the team's too. Relays carry encrypted traffic
only, when no direct path exists.

| Option | Meaning |
|---|---|
| `--clear` | Clear the relay you set. |
| `--local` | Only this computer, not the team (founder and admins set the team's by default). |

### version

```
lightbulb version
lightbulb --version
```

Prints the version, for example `lightbulb 0.2.2`.

## Commands the agents and macOS run

`hook`, `mcp` and `daemon` are not for typing. The agents run `lightbulb hook <agent> <event>`
and `lightbulb mcp`; macOS's login service runs `lightbulb daemon`. `setup` wires them.
