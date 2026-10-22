//! Codex: hooks in `~/.codex/hooks.json`, trusted in `~/.codex/config.toml`
//! (`[hooks.state."<hooks.json>:<event>:<group>:0"]`; Codex skips an untrusted group
//! silently), and `[mcp_servers.<name>]` in the same `config.toml`. Adapted from the
//! Lightbulb desktop app's local-session enrollment.

use super::{
    edit_json, edit_toml, set_hook_group, set_toml_server, toml_table, AgentReport, Ctx, Owner,
};
use crate::hook::{Event, EVENTS};
use crate::Result;
use serde_json::json;
use sha2::{Digest, Sha256};

pub(super) fn configure(ctx: &Ctx, r: &mut AgentReport) -> Result<()> {
    let dir = ctx.home.join(".codex");
    let hooks = dir.join("hooks.json");
    // Trust key -> the hash to trust, or None to drop the key.
    let mut trust: Vec<(String, Option<String>)> = vec![];
    let mut shared = vec![];
    edit_json(&hooks, !ctx.remove, r, |doc| {
        for e in EVENTS {
            let timeout = if e == Event::SessionEnd { 1 } else { 5 };
            let command = ctx.hook_command("codex", e.arg());
            let label = e.arg().replace('-', "_");
            let group = (!ctx.remove).then(|| {
                json!({"hooks": [{"type": "command", "command": command,
                    "timeout": timeout, "async": false}]})
            });
            match set_hook_group(ctx, doc, e.name(), group)? {
                Some(i) => {
                    let hash = (!ctx.remove).then(|| trust_hash(&label, &command, timeout, false));
                    trust.push((format!("{}:{label}:{i}:0", hooks.display()), hash));
                }
                // Installed into a group another tool's hook is in: its trust is not ours to write.
                None if !ctx.remove => shared.push(format!(
                    "Codex: the {} hook shares a group with another tool's; Codex will ask you to trust it once (/hooks)",
                    e.name()
                )),
                None => {}
            }
        }
        Ok(())
    })?;
    r.notes.append(&mut shared);
    let config = dir.join("config.toml");
    let mut owner = Owner::Absent;
    edit_toml(&config, !ctx.remove, r, |doc| {
        let args = toml_edit::Array::from_iter(["mcp"]);
        owner = set_toml_server(
            ctx,
            doc,
            &[("command", ctx.exe.into()), ("args", args.into())],
        )?;
        for (key, hash) in &trust {
            match hash {
                Some(hash) => {
                    let state = toml_table(
                        toml_table(doc.as_table_mut(), "hooks", true)?,
                        "state",
                        true,
                    )?;
                    toml_table(state, key, false)?["trusted_hash"] = toml_edit::value(hash);
                }
                None => {
                    if let Some(state) = doc
                        .get_mut("hooks")
                        .and_then(|h| h.get_mut("state"))
                        .and_then(toml_edit::Item::as_table_like_mut)
                    {
                        state.remove(key);
                    }
                }
            }
        }
        Ok(())
    })?;
    if owner == Owner::Foreign {
        r.notes.push(ctx.foreign_note(&config));
    }
    if !ctx.remove && !trust.is_empty() && r.wrote.contains(&config) {
        r.notes
            .push("Codex hooks trusted on your behalf (Codex would otherwise ask once)".into());
    }
    Ok(())
}

/// Codex's hook-trust hash: `{event_name, hooks: [hook]}` with keys sorted, compact JSON,
/// SHA-256, `sha256:<hex>`. (serde_json's map is sorted by key here.)
pub(super) fn trust_hash(label: &str, command: &str, timeout: u64, is_async: bool) -> String {
    let ident = json!({"event_name": label, "hooks": [{"type": "command", "command": command,
        "timeout": timeout, "async": is_async}]});
    let digest = Sha256::digest(ident.to_string());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn trust_hash_matches_what_codex_trusted() {
        // Read from a real ~/.codex/config.toml, for the desktop app's own hook.
        let command = "'/Applications/Lightbulb.app/Contents/MacOS/lightbulb-desktop' local-session-hook codex";
        assert_eq!(
            super::trust_hash("session_start", command, 5, true),
            "sha256:034e9d5ef64a6d0667082ae51352a4e032c35767d5fb96b507520ca8a9b16e1a"
        );
    }
}
