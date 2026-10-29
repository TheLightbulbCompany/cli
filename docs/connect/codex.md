# Connect Codex

## Install

```sh
curl -fsSL https://github.com/TheLightbulbCompany/cli/releases/latest/download/install.sh | sh
```

If setup prints an `export PATH=...` line, add it to `~/.zshrc` and open a new terminal.

## What setup wires

- `~/.codex/hooks.json`: hooks for the five events.
- `~/.codex/config.toml`: the `lightbulb` tool server with `default_tools_approval_mode = "approve"`
  (so Codex runs our tools without asking, also in `codex exec`), and Codex's trust for our hooks.
- `~/.codex/skills/lightbulb/SKILL.md`: a skill that teaches the tools.

## What you may need to do

Restart open Codex sessions. Setup trusts its own hooks, so Codex does not ask. If setup says a
hook shares a group with another tool's hook, open `/hooks` in Codex and trust it once.

## Try it

Say to Codex: `Use the lightbulb tools to find my sessions and send one a message.`

## Troubleshooting

Run `lightbulb doctor`. The Codex line should read `connected`.

- `not installed`: fine, Codex was not found on this computer.
- `hooks only — tools missing (run setup)`: run `lightbulb setup`, then restart Codex.
- `not connected`: run `lightbulb setup`. If it names a file it could not read, fix that file first.
- `MCP tool call requires approval, but approval policy is never`: an older setup did not
  pre-approve our tools. Run `lightbulb setup --refresh`.
- `This app tool requires a non-empty string link_id argument`: Codex called the Lightbulb ChatGPT
  app, which has tools with the same names, instead of this tool server. Say "the lightbulb tool
  server", or turn the app off for Codex: `[apps.<app id>]` with `enabled = false` in
  `~/.codex/config.toml` (the id is the app's `connector_id`, `asdk_app_...`).
- `source_unidentified` from a tool: Lightbulb could not tell which session called it. Restart the session.
- Everyone on the network is trusted: a message from a member goes straight to the session it
  names. Check it with `lightbulb receipt <id>`.

More: [Troubleshooting](../troubleshooting.md), [Agents](../agents.md).
