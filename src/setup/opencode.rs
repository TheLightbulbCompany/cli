//! OpenCode: it has no command hooks, so our plugin `~/.config/opencode/plugins/lightbulb.js`
//! runs `hook opencode <event>` for it; the MCP server is `mcp.<name>` in
//! `~/.config/opencode/opencode.json`, or in `opencode.jsonc` when that exists (edited only
//! while it is plain JSON: comments are never rewritten away). Written from
//! opencode.ai/docs/{plugins,config,mcp-servers,sdk} and OpenCode's own types at v1.18.32
//! (the URLs are in the plugin).

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

/// The plugin's source, with this executable in it.
pub(super) fn plugin_js(ctx: &Ctx) -> String {
    // The executable appears only in this JSON string literal (valid JS for any path), never
    // in a comment a line break in the path could end.
    let exe = serde_json::to_string(ctx.exe).expect("a string");
    format!(
        r#"// Lightbulb: written by `lightbulb setup` and deleted by `lightbulb setup --remove`;
// edits here are overwritten. For each OpenCode session that is not a subagent's it runs
//   EXE hook opencode session-start   (on session.created)
//   EXE hook opencode post-tool-use   (on tool.execute.after)
//   EXE hook opencode stop            (on session.idle)
//   EXE hook opencode session-end     (on session.deleted)
// with {{session_id, cwd, mail}} on stdin, and puts the text a hook offers into the session.
// The offer leaves the mail waiting; only once the prompt is in does it run
//   EXE hook opencode ack             ({{session_id, ids}} on stdin)
// so a push that fails is offered again at the next step (a resend may repeat a message,
// none is lost). A hook that fails or takes over 10 s is ignored: OpenCode is never held up.
// Mail goes in only with the session's own agent and model (seen in chat.message), so a
// teammate's message can never move a session from plan to build or change its model. Until
// they are seen, `mail: false` leaves the mail waiting for check_inbox.
// Event and hook shapes, from OpenCode v1.18.32:
// https://github.com/anomalyco/opencode/blob/v1.18.32/packages/sdk/js/src/gen/types.gen.ts
//   (session.created/deleted carry properties.info: Session {{id, parentID?}};
//    session.idle carries properties.sessionID)
// https://github.com/anomalyco/opencode/blob/v1.18.32/packages/plugin/src/index.ts
//   ("tool.execute.after" input: {{tool, sessionID, callID, args}};
//    "chat.message" input: {{sessionID, agent?, model?, variant?}}, output: {{message: UserMessage}},
//    UserMessage {{agent, model: {{providerID, modelID}}}} in types.gen.ts above)
// https://github.com/anomalyco/opencode/blob/v1.18.32/packages/opencode/src/session/prompt.ts
//   (a prompt with no agent runs the default agent; with noReply it is only saved)
import {{ spawn }} from "node:child_process"

const EXE = {exe}

export const Lightbulb = async ({{ client, directory }}) => {{
  // Session id -> whether it is a subagent's (has a parentID).
  const child = new Map()
  // Session id -> {{agent, model, variant?}} of its latest user message.
  const who = new Map()
  const isChild = async (id) => {{
    if (!child.has(id)) {{
      try {{
        const r = await client.session.get({{ path: {{ id }} }})
        child.set(id, !!r?.data?.parentID)
      }} catch {{
        return false
      }}
    }}
    return child.get(id)
  }}
  // Sessions with an offer in flight: they are not offered mail again until it is acked.
  const pushing = new Set()
  // Runs one hook; resolves to what it prints ({{text, ids}} when it offers mail), or null.
  const hook = (arg, id, mail, extra) =>
    new Promise((done) => {{
      let out = ""
      try {{
        const p = spawn(EXE, ["hook", "opencode", arg], {{ stdio: ["pipe", "pipe", "ignore"] }})
        p.stdout.setEncoding("utf8")
        const timer = setTimeout(() => {{
          try {{ p.kill() }} catch {{}}
          done(null)
        }}, 10000)
        p.stdout.on("data", (d) => (out += d))
        p.on("error", () => {{
          clearTimeout(timer)
          done(null)
        }})
        p.on("close", () => {{
          clearTimeout(timer)
          try {{
            done(out ? JSON.parse(out) : null)
          }} catch {{
            done(null)
          }}
        }})
        p.stdin.on("error", () => {{}})
        p.stdin.end(JSON.stringify({{ session_id: id, cwd: directory, mail, ...extra }}))
      }} catch {{
        done(null)
      }}
    }})
  // Whether the text went in. noReply: it joins the session without starting a reply (the
  // running turn reads it). The SDK answers an HTTP error as {{error}} rather than throwing.
  const say = async (id, text, noReply, w) => {{
    try {{
      const r = await client.session.prompt({{
        path: {{ id }},
        body: {{ parts: [{{ type: "text", text }}], ...w, ...(noReply ? {{ noReply }} : {{}}) }},
      }})
      return !r?.error
    }} catch {{
      return false
    }}
  }}
  // One step: the hook registers it, and offers mail only to a session whose agent and model
  // are known and which has no offer in flight; mail that went in is acked.
  const step = async (arg, id, noReply) => {{
    const w = who.get(id)
    if (!w || pushing.has(id)) return void (await hook(arg, id, false))
    pushing.add(id)
    try {{
      const offer = await hook(arg, id, true)
      const ids = offer?.ids
      if (typeof offer?.text !== "string" || !Array.isArray(ids)) return
      if (await say(id, offer.text, noReply, w)) await hook("ack", id, false, {{ ids }})
    }} finally {{
      pushing.delete(id)
    }}
  }}
  return {{
    "chat.message": async (input, output) => {{
      try {{
        const m = output?.message
        if (!input?.sessionID || !m?.agent || !m?.model) return
        who.set(input.sessionID, {{
          agent: m.agent,
          model: m.model,
          ...(input.variant ? {{ variant: input.variant }} : {{}}),
        }})
      }} catch {{}}
    }},
    event: async ({{ event }}) => {{
      try {{
        const props = event?.properties ?? {{}}
        if (event.type === "session.created" || event.type === "session.deleted") {{
          const info = props.info ?? {{}}
          if (!info.id) return
          child.set(info.id, !!info.parentID)
          if (info.parentID) return
          const arg = event.type === "session.created" ? "session-start" : "session-end"
          await hook(arg, info.id, false)
          if (event.type === "session.deleted") {{
            child.delete(info.id)
            who.delete(info.id)
          }}
        }} else if (event.type === "session.idle") {{
          const id = props.sessionID
          if (!id || (await isChild(id))) return
          // Starts a turn that is acked when it ends; not awaited, so the event is not held for
          // the whole reply.
          step("stop", id, false).catch(() => {{}})
        }}
      }} catch {{}}
    }},
    "tool.execute.after": async (input) => {{
      try {{
        const id = input?.sessionID
        if (!id || (await isChild(id))) return
        await step("post-tool-use", id, true)
      }} catch {{}}
    }},
  }}
}}
"#
    )
}
