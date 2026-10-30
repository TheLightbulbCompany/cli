//! Claude Code: hooks in `~/.claude/settings.json`, the user-scope MCP server in
//! `~/.claude.json`.

use super::{edit_json, set_hook_group, set_json_server, AgentReport, Ctx, Owner};
use crate::hook::{Event, EVENTS};
use crate::Result;
use serde_json::json;

pub(super) fn configure(ctx: &Ctx, r: &mut AgentReport) -> Result<Owner> {
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
                let timeout = match e {
                    Event::SessionEnd => 1,
                    Event::PermissionRequest => 2,
                    _ => 5,
                };
                let group = (!ctx.remove).then(|| {
                    json!({"hooks": [{"type": "command",
                        "command": ctx.hook_command("claude", e.arg()), "timeout": timeout}]})
                });
                set_hook_group(ctx, doc, e.name(), group)?;
            }
            deny_hosted(doc, !ctx.remove);
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
    super::skill(ctx, &ctx.home.join(".claude"), r)?;
    Ok(owner)
}

/// The hosted MCP, added on claude.ai as a connector, would also load in Claude Code beside this
/// server with the same tools. Claude Code's `deniedMcpServers` keeps it out of Claude Code
/// only (claude.ai keeps it), matched by URL so any connector name is covered. `on` adds the
/// one entry, otherwise it is taken out; a person's own entries stay.
fn deny_hosted(doc: &mut serde_json::Value, on: bool) {
    let ours = json!({ "serverUrl": format!("{}*", super::HOSTED) });
    let Some(obj) = doc.as_object_mut() else {
        return;
    };
    let mut list: Vec<serde_json::Value> = obj
        .get("deniedMcpServers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    list.retain(|e| *e != ours);
    if on {
        list.push(ours);
    }
    if list.is_empty() {
        obj.remove("deniedMcpServers");
    } else {
        obj.insert("deniedMcpServers".into(), list.into());
    }
}
