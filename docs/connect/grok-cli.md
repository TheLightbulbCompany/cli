# Connect Grok CLI

## Install

```sh
curl -fsSL https://github.com/TheLightbulbCompany/cli/releases/latest/download/install.sh | sh
```

If setup prints an `export PATH=...` line, add it to `~/.zshrc` and open a new terminal.

## What setup wires

- `~/.grok/hooks/lightbulb.json`: hooks for the five events (a file of ours alone).
- `~/.grok/config.toml`: the `lightbulb-network` tool server, enabled, and the allow rule `MCPTool(lightbulb-network__*)`.

## What you may need to do

Open Grok in a project folder and trust that folder if Grok asks. Hooks may not run in an untrusted folder.
<!-- TODO: not confirmed from code: the exact Grok trusted-folder prompt and setting; the plan only says "hooks need a trusted folder". -->

## Try it

Say to Grok: `Use the lightbulb tools to find my sessions and send one a message.`

## Troubleshooting

Run `lightbulb doctor`. The Grok CLI line should read `connected`.

- `not installed`: fine, Grok CLI was not found on this computer.
- `hooks only — tools missing (run setup)`: run `lightbulb setup`, then restart Grok CLI.
- `not connected`: run `lightbulb setup`. If it names a file it could not read, fix that file first.
- `source_unidentified` from a tool: Lightbulb could not tell which session called it. Restart Grok.
- Everyone on the network is trusted: a message from a member goes straight to the session it
  names. Check it with `lightbulb receipt <id>`.

More: [Troubleshooting](../troubleshooting.md), [Agents](../agents.md).
