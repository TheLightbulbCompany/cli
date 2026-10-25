//! OpenCode: it has no command hooks, so our plugin `~/.config/opencode/plugins/lightbulb.js`
//! runs `hook opencode <event>` for it; the MCP server is `mcp.<name>` in
//! `~/.config/opencode/opencode.json`, or in `opencode.jsonc` when that exists (edited only
//! while it is plain JSON: comments are never rewritten away). Written from
//! opencode.ai/docs/{plugins,config,mcp-servers,sdk} and OpenCode's own sources at v1.18.32
//! and v2.1.4; one plugin loads on both (the URLs are in it).

use super::{edit_json, set_json_server, strip_jsonc, write, AgentReport, Ctx, Owner};
use crate::Result;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// The plugin, under the home folder. OpenCode loads every `plugins/*.{js,ts}` in its config
/// folder, so this file is ours alone: written whole, deleted on remove, never backed up.
pub(super) const PLUGIN: &str = ".config/opencode/plugins/lightbulb.js";

/// The config file setup edits, under the home folder: `opencode.jsonc` if there is one
/// (never an `opencode.json` beside it), else `opencode.json`.
pub(super) fn config(home: &Path) -> PathBuf {
    let jsonc = Path::new(".config/opencode/opencode.jsonc");
    if home.join(jsonc).exists() {
        jsonc.into()
    } else {
        ".config/opencode/opencode.json".into()
    }
}

pub(super) fn configure(ctx: &Ctx, r: &mut AgentReport) -> Result<Owner> {
    let plugin = ctx.home.join(PLUGIN);
    if ctx.remove {
        if plugin.exists() {
            fs::remove_file(&plugin)?;
            r.wrote.push(plugin);
        }
    } else {
        let text = plugin_js(ctx);
        if fs::read_to_string(&plugin).ok().as_deref() != Some(text.as_str()) {
            write(&plugin, &text, false, r)?;
        }
    }
    let mut command = vec![ctx.exe];
    command.extend(ctx.mcp_args());
    let entry = json!({"type": "local", "command": command, "enabled": true});
    let path = ctx.home.join(config(ctx.home));
    if let Ok(text) = fs::read_to_string(&path) {
        if serde_json::from_str::<Value>(&text).is_err()
            && path.extension() == Some("jsonc".as_ref())
        {
            return commented(ctx, &path, &text, &entry);
        }
    }
    let mut owner = Owner::Absent;
    edit_json(&path, !ctx.remove, r, |doc| {
        owner = set_json_server(ctx, doc, "mcp", entry)?;
        Ok(())
    })?;
    Ok(owner)
}

/// An `opencode.jsonc` with comments or trailing commas, which setup never rewrites: our
/// entry there already (pasted by the person) counts; otherwise the person is told what to
/// paste in or take out.
fn commented(ctx: &Ctx, path: &Path, text: &str, entry: &Value) -> Result<Owner> {
    let doc = serde_json::from_str::<Value>(&strip_jsonc(text))
        .map_err(|e| format!("{} has a syntax error ({e})", path.display()))?;
    let current = &doc["mcp"][ctx.server];
    let ours = super::json_command(current) == Some(ctx.exe);
    let left = format!("{} has comments, so setup left it alone", path.display());
    match (ctx.remove, ours) {
        (false, true) if current == entry => Ok(Owner::Ours),
        (true, false) => Ok(Owner::Absent),
        (false, _) => Err(format!(
            "{left}; add this inside its \"mcp\" object yourself: \"{}\": {entry}",
            ctx.server
        )
        .into()),
        (true, true) => {
            Err(format!("{left}; take \"{}\" out of \"mcp\" yourself", ctx.server).into())
        }
    }
}

/// The plugin's source (`opencode-plugin.js`), with this executable in it: only as a JSON
/// string literal (valid JS for any path), never in a comment a line break in the path could end.
pub(super) fn plugin_js(ctx: &Ctx) -> String {
    let exe = serde_json::to_string(ctx.exe).expect("a string");
    include_str!("opencode-plugin.js").replacen("__EXE__", &exe, 1)
}
