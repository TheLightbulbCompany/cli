# Connect Claude Code

## Install

```sh
curl -fsSL https://github.com/TheLightbulbCompany/cli/releases/latest/download/install.sh | sh
```

If setup prints an `export PATH=...` line, add it to `~/.zshrc` and open a new terminal.

## What setup wires

- `~/.claude/settings.json`: hooks for session start, prompt, tool call, stop and session end.
- `~/.claude.json`: the `lightbulb` tool server.
- `~/.claude/skills/lightbulb-network/SKILL.md`: a skill that teaches the tools.

Each file is backed up once as `<file>.lightbulb-backup`.

## What you may need to do

Restart any Claude Code session that was already open, so it loads the tool server.
If `disableAllHooks` is set in `settings.json`, turn it off; setup warns about this.

## Try it

Say to Claude Code: `Use the lightbulb tools to find my sessions and send one a message.`

## Troubleshooting

Run `lightbulb doctor`. The Claude Code line should read `connected`.

- `not installed`: fine, Claude Code was not found on this computer.
- `hooks only — tools missing (run setup)`: run `lightbulb setup`, then restart Claude Code.
- `not connected`: run `lightbulb setup`. If it names a file it could not read, fix that file first.
- `source_unidentified` from a tool: Lightbulb could not tell which session called it. Restart the session.
- Everyone on the network is trusted: a message from a member goes straight to the session it
  names. Check it with `lightbulb receipt <id>`.

More: [Troubleshooting](../troubleshooting.md), [Agents](../agents.md).
