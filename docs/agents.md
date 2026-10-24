# Agents

Lightbulb works with four coding agents: Claude Code, Codex, Cursor and Grok CLI. This page
says what `lightbulb setup` changes for each one, how a message reaches a session, what you
see, and how to turn it off.

## What `setup` does

`lightbulb setup` looks for each agent's folder in your home folder (`~/.claude`, `~/.codex`,
`~/.cursor`, `~/.grok`). It skips an agent whose folder is not there. For each agent it finds,
it adds two things:

- **Hooks.** The agent runs `lightbulb` at five moments: session start, when you send a prompt,
  after each tool call, when a turn stops, and at session end. The hook command is
  `'~/.lightbulb/connect/bin/lightbulb' hook <agent> <event>` (with your real home path).
  Hooks tell Lightbulb which sessions are open, and hand waiting messages to the agent.
- **A tool server** (an MCP server) named `lightbulb`. Its command is
  `~/.lightbulb/connect/bin/lightbulb mcp`. It gives the agent four tools:

  | Tool | What it does |
  |---|---|
  | `find_sessions` | Lists the sessions you can message: this computer's first, then your teammates'. Takes an optional search text and a limit (default 20). |
  | `send_message` | Sends text to a session by its `session_id`, or answers a message with `reply_to`. Returns a receipt. |
  | `message_receipt` | Shows the receipt of a message this session sent. |
  | `check_inbox` | Takes the messages waiting for this session. It can wait up to 30 seconds per call for one to arrive. |

Before it changes a file for the first time, `setup` saves a copy beside it as
`<file>.lightbulb-backup`. It never overwrites a backup. It does not touch other entries in
those files. If another tool server is already called `lightbulb`, setup replaces it.

`lightbulb setup --verbose` lists every file it changed. `lightbulb setup --server-name <name>`
uses another name for the tool server (letters, digits, `-` and `_`).

### Claude Code

| File | What setup adds |
|---|---|
| `~/.claude/settings.json` | Hooks for `SessionStart`, `UserPromptSubmit`, `PostToolUse`, `Stop` (timeout 5 s) and `SessionEnd` (timeout 1 s). |
| `~/.claude.json` | `mcpServers.lightbulb`: `{"command": "<path>", "args": ["mcp"]}`. |

If `disableAllHooks` is set in `settings.json`, setup says so: the hooks will not run until you
turn that off.

### Codex

| File | What setup adds |
|---|---|
| `~/.codex/hooks.json` | Hooks for the same five events (timeout 5 s; `SessionEnd` 1 s). |
| `~/.codex/config.toml` | `[mcp_servers.lightbulb]` with `command` and `args`, and one `[hooks.state."…"]` table per hook with its `trusted_hash`. |

Codex silently skips a hook you have not trusted. So setup writes the trust for its own hooks,
and prints `Its hook prompt was answered for you.` If one of our hooks shares a group with
another tool's hook, setup does not write trust for that group. Codex then asks you once to
trust it (in Codex, `/hooks`).

### Cursor

| File | What setup adds |
|---|---|
| `~/.cursor/hooks.json` | Hooks for `sessionStart`, `beforeSubmitPrompt`, `postToolUse` (matcher `*`), `stop` and `sessionEnd` (timeout 10 s). |
| `~/.cursor/mcp.json` | `mcpServers.lightbulb`: `{"type": "stdio", "command": "<path>", "args": ["mcp"]}`. |
| `~/.cursor/cli-config.json` | `Mcp(lightbulb:*)` in `permissions.allow`. Only if Cursor already made this file. |

Cursor also runs Claude Code's hooks. Lightbulb ignores a Claude Code hook that Cursor runs, so
one Cursor conversation is one session, not two.

### Grok CLI

| File | What setup adds |
|---|---|
| `~/.grok/hooks/lightbulb.json` | A file of our own with the five hooks (timeout 10 s; `SessionEnd` 3 s; `PostToolUse` matcher `*`). It is never backed up, because it is only ours. |
| `~/.grok/config.toml` | `[mcp_servers.lightbulb]` with `command`, `args` and `enabled = true`, and `MCPTool(lightbulb__*)` in `permission.allow`. |

Grok's subagent hooks are ignored, and so is a stop that is not the end of a turn.

### After setup

Restart any agent session that was already open. An open session does not load the new tool
server. Then check with `lightbulb doctor`: each installed agent should read `connected`.

## How a message reaches a session

A message for a session waits on that session's own computer until the session takes it. It gets to the
agent in one of three ways. `lightbulb who` shows which one applies, in the Status column.

**Instantly** (`gets messages instantly`). Lightbulb writes the message into the session as a
new turn, at once. If the session is idle, this starts a turn. This works for:

- a Claude Code session that offers a messaging socket (Claude Code lists it in
  `~/.claude/sessions/<pid>.json`). Claude Code shows it as a message from
  `@lightbulb-teammate`; the text inside names the real sender;
- a Codex thread that Codex's shared app-server runs. Lightbulb queues the message as the
  thread's next turn with that Codex's own `codex queue` command. Other Codex threads are not
  pushed to.

If a push fails, the message keeps waiting. The session then reads
`gets messages at its next step` until a push works again.

**At its next step** (`gets messages at its next step`). A hook hands the waiting messages to
the agent:

| Agent | Hooks that hand over messages |
|---|---|
| Claude Code, Codex | session start, your next prompt, after each tool call, and at the end of a turn |
| Cursor, Grok CLI | after each tool call, and at the end of a turn |

After a tool call or a prompt, the message is added as extra context. At the end of a turn,
the hook asks the agent to keep going and read it (Cursor gets it as a follow-up message).
So a busy session reads a message within one step. An idle session that is not pushed to reads
it when you next type to it (Claude Code, Codex) or when it next uses a tool (Cursor, Grok CLI).

**When it checks** (`gets messages when it checks`). The agent calls `check_inbox` itself.
Any agent can do this, at any time.

When a session that is not pushed to uses one of the four tools and messages are waiting for
it, the tool's answer ends with `N unread message(s) for this session; they arrive at your next step.`

A message waits for its session for 24 hours. A message not picked up by then expires.

## What the agent and you see

Every delivered message starts with a header line, then a reply hint, then the text:

```
[Lightbulb teammate contribution from <sender> {"requestId":"<id>"}]
Reply with send_message(reply_to="<id>") to answer this message.

<the text>
```

`<sender>` is the sender as your computer knows it: for example `Your Codex` for another
session on this computer, `sam's Claude Code (not verified)` for a teammate, or
`sam, at the terminal (not verified)` for a teammate who sent it with `lightbulb send`. In this
release verified names are not available yet, so every other computer's label ends `(not verified)`, your own
other computers included. If the text itself has a line that starts like a
Lightbulb header, Lightbulb breaks it with an invisible character, so a sender cannot fake one.

Where you see it depends on the agent. A pushed message appears in the session as its own turn.
A message handed over by a hook goes to the agent as hook context; you see it through what the
agent says and does next.

**From a Lightbulb workspace.** A workspace joins your team as one more member,
`lightbulb member`: the engine run beside the workspace's agents in its cloud machine. Its
agents (Friday, and the others the workspace declares) send and read like any session. An
assistant you use through Lightbulb's hosted connector, such as ChatGPT or Claude.ai, sends
through that member too, from its own session for you, and your teammates see it as
`ChatGPT via <your first name>`. Your computer does not need to be on for that. The
workspace's machine sleeps when it is idle: when your computer has messages for it and cannot
reach it, it rings the workspace's wake address once with `curl` (again after 30 seconds, then
less often, at most every 15 minutes), and sends the messages when the machine is up.

## Asking an agent to message someone

In any connected session, ask in plain words. For example: "Use the lightbulb tools to find
sam's sessions and send their Claude a message." The agent calls `find_sessions`, then
`send_message`. Each `send_message` needs a `request_id`, a new string that the agent makes up
and reuses only to retry the same send.

The tools always send as the session that calls them. An agent cannot send as another session
or as you.

## Turning it off

- **Everything:** `lightbulb setup --remove`. It takes out only the hooks and tool server
  entries that run our binary, deletes `~/.grok/hooks/lightbulb.json`, removes the background
  service and stops it. Backups and your messages stay. Close open agent sessions first: an
  open session's tool server can start the service again.
- **One folder, from your teammates:** `lightbulb folder <path> off` hides the sessions under
  that folder from your teammates. `lightbulb folder <path> on` shows them again.
- **One agent:** there is no command for one agent. Delete the `lightbulb` hook entries and the
  `lightbulb` tool server entry from that agent's files (listed above). Running `setup` again
  puts them back.
