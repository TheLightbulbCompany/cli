//! Cursor: hooks in `~/.cursor/hooks.json` (`{version: 1, hooks: {<event>: [{command,
//! timeout, matcher?}]}}`), the MCP server in `~/.cursor/mcp.json`, and the allow rule in
//! `~/.cursor/cli-config.json`'s `permissions.allow` (created when missing).
//! Written from those files' formats, cursor.com/docs/hooks and the allow-list syntax
//! `Mcp(server:tool)` (either part may be `*`) in cursor.com/docs/cli/reference/permissions.

use super::{edit_json, set_json_rule, set_json_server, AgentReport, Ctx, Owner, OLD_SERVER};
use crate::hook::{Event, EVENTS};
use crate::Result;
use serde_json::{json, Value};
use std::fs;

fn cursor_event(e: Event) -> &'static str {
    match e {
        Event::SessionStart => "sessionStart",
        Event::UserPromptSubmit => "beforeSubmitPrompt",
        Event::PostToolUse => "postToolUse",
        Event::Stop => "stop",
        Event::SessionEnd => "sessionEnd",
        Event::PermissionRequest | Event::Notification => unreachable!("not installed for Cursor"),
    }
}

pub(super) fn configure(ctx: &Ctx, r: &mut AgentReport) -> Result<Owner> {
    let dir = ctx.home.join(".cursor");
    let hooks = dir.join("hooks.json");
    edit_json(&hooks, !ctx.remove, r, |doc| {
        if !ctx.remove && doc.get("version").is_none() {
            doc["version"] = json!(1);
        }
        for e in EVENTS.into_iter().filter(|e| !e.waits()) {
            let mut entry = json!({"command": ctx.hook_command("cursor", e.arg()), "timeout": 10});
            if e == Event::PostToolUse {
                entry["matcher"] = json!("*");
            }
            set_entry(ctx, doc, cursor_event(e), (!ctx.remove).then_some(entry))?;
        }
        Ok(())
    })?;
    // Removed down to nothing but the version: the file was only ours.
    if ctx.remove
        && fs::read_to_string(&hooks)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            == Some(json!({"version": 1}))
    {
        fs::remove_file(&hooks)?;
    }
    let mcp = dir.join("mcp.json");
    let mut owner = Owner::Absent;
    edit_json(&mcp, !ctx.remove, r, |doc| {
        let entry = json!({"type": "stdio", "command": ctx.exe, "args": ctx.mcp_args()});
        owner = set_json_server(ctx, doc, "mcpServers", entry)?;
        Ok(())
    })?;
    if owner == Owner::Foreign {
        return Ok(owner);
    }
    let rule = format!("Mcp({}:*)", ctx.server);
    edit_json(&dir.join("cli-config.json"), !ctx.remove, r, |doc| {
        if owner == Owner::Ours && doc.as_object().is_some_and(|o| o.is_empty()) {
            doc["version"] = json!(1);
        }
        set_json_rule(doc, "permissions", "allow", &rule, owner == Owner::Ours);
        if ctx.server != OLD_SERVER {
            set_json_rule(
                doc,
                "permissions",
                "allow",
                &format!("Mcp({OLD_SERVER}:*)"),
                false,
            );
        }
        Ok(())
    })?;
    Ok(owner)
}

/// Our entry in `hooks.<event>`: replaced in place or appended; removed when `None`.
fn set_entry(ctx: &Ctx, doc: &mut Value, event: &str, entry: Option<Value>) -> Result<()> {
    let ours = |h: &Value| h["command"].as_str().is_some_and(|c| ctx.is_ours(c));
    let root = doc.as_object_mut().ok_or("not an object")?;
    if entry.is_none() && root.get("hooks").and_then(|h| h.get(event)).is_none() {
        return Ok(());
    }
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("`hooks` is not an object")?;
    let list = hooks
        .entry(event)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| format!("`hooks.{event}` is not a list"))?;
    match (entry, list.iter().position(ours)) {
        (Some(e), Some(i)) => list[i] = e,
        (Some(e), None) => list.push(e),
        (None, _) => list.retain(|h| !ours(h)),
    }
    if list.is_empty() {
        hooks.remove(event);
    }
    if hooks.is_empty() {
        root.remove("hooks");
    }
    Ok(())
}
