//! Cursor: hooks in `~/.cursor/hooks.json` (`{version: 1, hooks: {<event>: [{command,
//! timeout, matcher?}]}}`), the MCP server in `~/.cursor/mcp.json`, and the allow rule in
//! `~/.cursor/cli-config.json`'s `permissions.allow` (edited only if Cursor made the file).
//! Written from those files' formats and cursor.com/docs/agent/hooks.

use super::{edit_json, set_json_rule, set_json_server, AgentReport, Ctx, Owner};
use crate::hook::{Event, EVENTS};
use crate::Result;
use serde_json::{json, Value};

fn cursor_event(e: Event) -> &'static str {
    match e {
        Event::SessionStart => "sessionStart",
        Event::UserPromptSubmit => "beforeSubmitPrompt",
        Event::PostToolUse => "postToolUse",
        Event::Stop => "stop",
        Event::SessionEnd => "sessionEnd",
    }
}

pub(super) fn configure(ctx: &Ctx, r: &mut AgentReport) -> Result<()> {
    let dir = ctx.home.join(".cursor");
    edit_json(&dir.join("hooks.json"), !ctx.remove, r, |doc| {
        if !ctx.remove && doc.get("version").is_none() {
            doc["version"] = json!(1);
        }
        for e in EVENTS {
            let mut entry = json!({"command": ctx.hook_command("cursor", e.arg()), "timeout": 10});
            if e == Event::PostToolUse {
                entry["matcher"] = json!("*");
            }
            set_entry(ctx, doc, cursor_event(e), (!ctx.remove).then_some(entry))?;
        }
        Ok(())
    })?;
    let mcp = dir.join("mcp.json");
    let mut owner = Owner::Absent;
    edit_json(&mcp, !ctx.remove, r, |doc| {
        let entry = json!({"type": "stdio", "command": ctx.exe, "args": ["mcp"]});
        owner = set_json_server(ctx, doc, "mcpServers", entry)?;
        Ok(())
    })?;
    if owner == Owner::Foreign {
        r.notes.push(ctx.foreign_note(&mcp));
        return Ok(());
    }
    let rule = format!("Mcp({}:*)", ctx.server);
    edit_json(&dir.join("cli-config.json"), false, r, |doc| {
        set_json_rule(doc, "permissions", "allow", &rule, owner == Owner::Ours);
        Ok(())
    })
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
