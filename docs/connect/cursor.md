# Connect Cursor

## Install

```sh
curl -fsSL https://github.com/TheLightbulbCompany/cli/releases/latest/download/install.sh | sh
```

If setup prints an `export PATH=...` line, add it to `~/.zshrc` and open a new terminal.

## What setup wires

- `~/.cursor/hooks.json`: hooks for the five events.
- `~/.cursor/mcp.json`: the `lightbulb` tool server.
- `~/.cursor/cli-config.json`: the allow rule `Mcp(lightbulb:*)`, only if Cursor already made this file.

## What you may need to do

Restart Cursor. If Cursor asks to approve a `lightbulb` tool call, approve it.

## Try it

Say to Cursor: `Use the lightbulb tools to find my sessions and send one a message.`

## Troubleshooting

Run `lightbulb doctor`. The Cursor line should read `connected`.

- `not installed`: fine, Cursor was not found on this computer.
- `hooks only — tools missing (run setup)`: run `lightbulb setup`, then restart Cursor.
- `not connected`: run `lightbulb setup`. If it names a file it could not read, fix that file first.
- `source_unidentified` from a tool: Lightbulb could not tell which session called it. Restart Cursor.
- Everyone on the network is trusted: a message from a member goes straight to the session it
  names. Check it with `lightbulb receipt <id>`.

More: [Troubleshooting](../troubleshooting.md), [Agents](../agents.md).
