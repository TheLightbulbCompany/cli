//! Claude Code: hooks in `~/.claude/settings.json`, the user-scope MCP server in
//! `~/.claude.json`.

use super::{edit_json, set_hook_group, set_json_server, AgentReport, Ctx, Owner};
use crate::hook::{Event, EVENTS};
use crate::Result;
use serde_json::json;

pub(super) fn configure(ctx: &Ctx, r: &mut AgentReport) -> Result<()> {
    let mut notes = vec![];
    edit_json(
        &ctx.home.join(".claude/settings.json"),
        !ctx.remove,
        r,
        |doc| {
            if !ctx.remove && doc["disableAllHooks"] == true {
                notes.push("Claude Code has disableAllHooks set; its hooks will not run".into());
            }
            for e in EVENTS {
                let timeout = if e == Event::SessionEnd { 1 } else { 5 };
                let group = (!ctx.remove).then(|| {
                    json!({"hooks": [{"type": "command",
                        "command": ctx.hook_command("claude", e.arg()), "timeout": timeout}]})
                });
                set_hook_group(ctx, doc, e.name(), group)?;
            }
            Ok(())
        },
    )?;
    r.notes.append(&mut notes);
    let config = ctx.home.join(".claude.json");
    let mut owner = Owner::Absent;
    edit_json(&config, !ctx.remove, r, |doc| {
        let entry = json!({"command": ctx.exe, "args": ctx.mcp_args()});
        owner = set_json_server(ctx, doc, "mcpServers", entry)?;
        Ok(())
    })?;
    if owner == Owner::Foreign {
        r.notes.push(ctx.foreign_note(&config));
    }
    Ok(())
}
