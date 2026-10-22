//! Grok CLI: hooks in `~/.grok/hooks/lightbulb.json` (Claude-shaped), and
//! `[mcp_servers.<name>]` plus `permission.allow = ["MCPTool(<name>__*)"]` in
//! `~/.grok/config.toml`. Written from those files' formats and the Grok CLI manual.

use super::{
    edit_json, edit_toml, set_hook_group, set_toml_server, toml_table, AgentReport, Ctx, Owner,
};
use crate::hook::{Event, EVENTS};
use crate::Result;
use serde_json::json;
use std::fs;

pub(super) fn configure(ctx: &Ctx, r: &mut AgentReport) -> Result<()> {
    let dir = ctx.home.join(".grok");
    let hooks = dir.join("hooks/lightbulb.json");
    edit_json(&hooks, !ctx.remove, r, |doc| {
        for e in EVENTS {
            let timeout = if e == Event::SessionEnd { 3 } else { 10 };
            let mut group = json!({"hooks": [{"type": "command",
                "command": ctx.hook_command("grok", e.arg()), "timeout": timeout}]});
            if e == Event::PostToolUse {
                group["matcher"] = json!("*");
            }
            set_hook_group(ctx, doc, e.name(), (!ctx.remove).then_some(group))?;
        }
        Ok(())
    })?;
    // Removed down to nothing: the file was only ours.
    if ctx.remove && fs::read_to_string(&hooks).is_ok_and(|t| t.trim() == "{}") {
        fs::remove_file(&hooks)?;
    }
    let config = dir.join("config.toml");
    let mut owner = Owner::Absent;
    edit_toml(&config, !ctx.remove, r, |doc| {
        let args = toml_edit::Array::from_iter(["mcp"]);
        let fields = [
            ("command", ctx.exe.into()),
            ("args", args.into()),
            ("enabled", true.into()),
        ];
        owner = set_toml_server(ctx, doc, &fields)?;
        if owner == Owner::Foreign {
            return Ok(());
        }
        let rule = format!("MCPTool({}__*)", ctx.server);
        let has = |doc: &toml_edit::DocumentMut| {
            doc.get("permission")
                .and_then(|p| p.get("allow"))
                .and_then(toml_edit::Item::as_array)
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(&rule)))
        };
        if owner == Owner::Ours && !has(doc) {
            let permission = toml_table(doc.as_table_mut(), "permission", false)?;
            let allow = permission
                .entry("allow")
                .or_insert_with(|| toml_edit::value(toml_edit::Array::new()))
                .as_array_mut()
                .ok_or("`permission.allow` is not a list")?;
            allow.push(rule.as_str());
        } else if owner == Owner::Absent && has(doc) {
            let allow = doc["permission"]["allow"]
                .as_array_mut()
                .expect("has the rule");
            allow.retain(|v| v.as_str() != Some(&rule));
        }
        Ok(())
    })?;
    if owner == Owner::Foreign {
        r.notes.push(ctx.foreign_note(&config));
    }
    Ok(())
}
