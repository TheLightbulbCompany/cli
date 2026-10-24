//! Every test runs on a temp home, never the real one.

use super::*;
use serde_json::json;
use std::path::Path;

const EXE: &str = "/opt/lb/bin/lightbulb";
const APP: &str = "/opt/other/bin/other-tool";
const EVENTS: [(&str, &str); 5] = [
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "user-prompt-submit"),
    ("PostToolUse", "post-tool-use"),
    ("Stop", "stop"),
    ("SessionEnd", "session-end"),
];

fn put(home: &Path, rel: &str, text: &str) {
    let p = home.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn get(home: &Path, rel: &str) -> String {
    fs::read_to_string(home.join(rel)).unwrap_or_default()
}

fn json_at(home: &Path, rel: &str) -> Value {
    serde_json::from_str(&get(home, rel)).unwrap()
}

fn toml_at(home: &Path, rel: &str) -> toml_edit::DocumentMut {
    get(home, rel).parse().unwrap()
}

fn setup(home: &Path, remove: bool) -> Report {
    run(home, Path::new(EXE), None, remove)
}

fn agent<'a>(r: &'a Report, name: &str) -> &'a AgentReport {
    r.agents.iter().find(|a| a.name == name).unwrap()
}

fn cmd(agent: &str, arg: &str) -> String {
    format!("'{EXE}' hook {agent} {arg}")
}

/// A user hook and another app's hook, which setup must keep.
fn others(event: &str) -> Value {
    json!({event: [{"hooks": [{"type": "command", "command": "echo mine"}]},
        {"hooks": [{"type": "command", "command": format!("'{APP}' local-session-hook x")}]}]})
}

/// `path` was backed up holding `original`, and a second run leaves that backup alone.
fn backed_up_once(home: &Path, rel: &str, original: &str) {
    let backup = backup_path(&home.join(rel));
    assert_eq!(fs::read_to_string(&backup).unwrap(), original, "{rel}");
    put(home, rel, &get(home, rel)); // unchanged contents, but a fresh run must not re-back-up
    setup(home, false);
    assert_eq!(fs::read_to_string(&backup).unwrap(), original, "{rel}");
}

#[test]
fn setup_writes_claude_hooks_and_mcp_and_backs_up_once() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let settings = json!({"model": "opus", "hooks": others("Stop")}).to_string();
    let config = json!({"numStartups": 3, "mcpServers": {"other": {"command": "x"}}}).to_string();
    put(h, ".claude/settings.json", &settings);
    put(h, ".claude.json", &config);

    let r = setup(h, false);
    let claude = agent(&r, "claude");
    assert_ne!(claude.state, State::NotInstalled);
    assert_eq!(
        claude.wrote,
        [
            h.join(".claude/settings.json"),
            h.join(".claude.json"),
            h.join(".claude/skills/lightbulb-network/SKILL.md")
        ]
    );

    let s = json_at(h, ".claude/settings.json");
    assert_eq!(s["model"], "opus");
    for (event, arg) in EVENTS {
        let timeout = if event == "SessionEnd" { 1 } else { 5 };
        let ours = json!({"hooks": [{"type": "command", "command": cmd("claude", arg), "timeout": timeout}]});
        assert!(
            s["hooks"][event].as_array().unwrap().contains(&ours),
            "{event}"
        );
    }
    assert_eq!(s["hooks"]["Stop"].as_array().unwrap().len(), 3);
    let c = json_at(h, ".claude.json");
    assert_eq!(
        c["mcpServers"]["lightbulb-network"],
        json!({"command": EXE, "args": ["mcp"]})
    );
    assert_eq!(c["mcpServers"]["other"], json!({"command": "x"}));
    assert_eq!(c["numStartups"], 3);

    // Idempotent.
    assert!(agent(&setup(h, false), "claude").wrote.is_empty());
    backed_up_once(h, ".claude/settings.json", &settings);
    backed_up_once(h, ".claude.json", &config);
}

#[test]
fn setup_writes_codex_hooks_and_mcp_and_backs_up_once() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let hooks = json!({"hooks": others("Stop")}).to_string();
    let config = "# mine\nmodel = \"gpt-5\"\n\n[mcp_servers.other]\ncommand = \"x\" # keep\n";
    put(h, ".codex/hooks.json", &hooks);
    put(h, ".codex/config.toml", config);

    let r = setup(h, false);
    assert_eq!(
        agent(&r, "codex").wrote,
        [
            h.join(".codex/hooks.json"),
            h.join(".codex/config.toml"),
            h.join(".codex/skills/lightbulb-network/SKILL.md")
        ]
    );
    let doc = json_at(h, ".codex/hooks.json");
    let text = get(h, ".codex/config.toml");
    assert!(text.starts_with(config), "{text}");
    let toml = toml_at(h, ".codex/config.toml");
    let path = h.join(".codex/hooks.json");
    for (event, arg) in EVENTS {
        let timeout = if event == "SessionEnd" { 1 } else { 5 };
        let command = cmd("codex", arg);
        let ours = json!({"hooks": [{"type": "command", "command": command, "timeout": timeout, "async": false}]});
        let index = doc["hooks"][event]
            .as_array()
            .unwrap()
            .iter()
            .position(|g| g == &ours)
            .unwrap();
        // The Stop group goes after the two already there.
        assert_eq!(index, if event == "Stop" { 2 } else { 0 });
        let label = arg.replace('-', "_");
        let key = format!("{}:{label}:{index}:0", path.display());
        assert_eq!(
            toml["hooks"]["state"][&key]["trusted_hash"].as_str(),
            Some(codex::trust_hash(&label, &command, timeout, false).as_str())
        );
    }
    let server = &toml["mcp_servers"]["lightbulb-network"];
    assert_eq!(server["command"].as_str(), Some(EXE));
    assert_eq!(
        server["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>(),
        ["mcp"]
    );

    assert!(agent(&setup(h, false), "codex").wrote.is_empty());
    backed_up_once(h, ".codex/hooks.json", &hooks);
    backed_up_once(h, ".codex/config.toml", config);
}

#[test]
fn setup_writes_cursor_hooks_and_mcp_and_backs_up_once() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let mcp = json!({"mcpServers": {"filesystem": {"command": "npx"}}}).to_string();
    let cli =
        json!({"version": 1, "permissions": {"allow": ["Shell(ls)"], "deny": []}}).to_string();
    put(h, ".cursor/mcp.json", &mcp);
    put(h, ".cursor/cli-config.json", &cli);

    let r = setup(h, false);
    assert_eq!(
        agent(&r, "cursor").wrote,
        [
            h.join(".cursor/hooks.json"),
            h.join(".cursor/mcp.json"),
            h.join(".cursor/cli-config.json")
        ]
    );
    let hooks = json_at(h, ".cursor/hooks.json");
    assert_eq!(hooks["version"], 1);
    for (event, arg) in [
        ("sessionStart", "session-start"),
        ("beforeSubmitPrompt", "user-prompt-submit"),
        ("postToolUse", "post-tool-use"),
        ("stop", "stop"),
        ("sessionEnd", "session-end"),
    ] {
        let mut ours = json!({"command": cmd("cursor", arg), "timeout": 10});
        if event == "postToolUse" {
            ours["matcher"] = json!("*");
        }
        assert_eq!(hooks["hooks"][event], json!([ours]), "{event}");
    }
    let m = json_at(h, ".cursor/mcp.json");
    assert_eq!(
        m["mcpServers"]["lightbulb-network"],
        json!({"type": "stdio", "command": EXE, "args": ["mcp"]})
    );
    assert_eq!(m["mcpServers"]["filesystem"], json!({"command": "npx"}));
    assert_eq!(
        json_at(h, ".cursor/cli-config.json")["permissions"]["allow"],
        json!(["Shell(ls)", "Mcp(lightbulb-network:*)"])
    );

    assert!(agent(&setup(h, false), "cursor").wrote.is_empty());
    backed_up_once(h, ".cursor/mcp.json", &mcp);
    backed_up_once(h, ".cursor/cli-config.json", &cli);
    // A file setup created had nothing to back up.
    assert!(!backup_path(&h.join(".cursor/hooks.json")).exists());
}

#[test]
fn cursor_cli_config_is_edited_never_created() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir(home.path().join(".cursor")).unwrap();
    setup(home.path(), false);
    assert!(!home.path().join(".cursor/cli-config.json").exists());
}

#[test]
fn setup_writes_grok_hooks_and_mcp_and_backs_up_once() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let config = "[cli]\ninstaller = \"npm\"\n\n[permission]\nallow = [ \"Bash(ls)\" ]\n";
    put(h, ".grok/config.toml", config);

    let r = setup(h, false);
    assert_eq!(
        agent(&r, "grok").wrote,
        [
            h.join(".grok/hooks/lightbulb.json"),
            h.join(".grok/config.toml")
        ]
    );
    let hooks = json_at(h, ".grok/hooks/lightbulb.json");
    for (event, arg) in EVENTS {
        let timeout = if event == "SessionEnd" { 3 } else { 10 };
        let mut ours = json!({"hooks": [{"type": "command", "command": cmd("grok", arg), "timeout": timeout}]});
        if event == "PostToolUse" {
            ours["matcher"] = json!("*");
        }
        assert_eq!(hooks["hooks"][event], json!([ours]), "{event}");
    }
    let toml = toml_at(h, ".grok/config.toml");
    let server = &toml["mcp_servers"]["lightbulb-network"];
    assert_eq!(server["command"].as_str(), Some(EXE));
    assert_eq!(server["enabled"].as_bool(), Some(true));
    let allow: Vec<_> = toml["permission"]["allow"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(allow, ["Bash(ls)", "MCPTool(lightbulb-network__*)"]);
    assert_eq!(toml["cli"]["installer"].as_str(), Some("npm"));

    assert!(agent(&setup(h, false), "grok").wrote.is_empty());
    backed_up_once(h, ".grok/config.toml", config);
}

#[test]
fn remove_takes_only_our_entries() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let app_server = json!({"command": APP, "args": ["serve"]});
    let json_files = [
        (
            ".claude/settings.json",
            json!({"model": "opus", "hooks": others("Stop")}),
        ),
        (".claude.json", json!({"mcpServers": {"other": app_server}})),
        (
            ".codex/hooks.json",
            json!({"hooks": others("SessionStart")}),
        ),
        (
            ".cursor/hooks.json",
            json!({"version": 1, "hooks": {"stop": [{"command": "echo mine"}]}}),
        ),
        (
            ".cursor/mcp.json",
            json!({"mcpServers": {"other": app_server}}),
        ),
        (
            ".cursor/cli-config.json",
            json!({"permissions": {"allow": ["Mcp(other:*)"]}}),
        ),
    ];
    for (rel, v) in &json_files {
        put(h, rel, &v.to_string());
    }
    let codex = "model = \"gpt-5\"\n\n[mcp_servers.other]\ncommand = \"x\"\n";
    let grok = "[permission]\nallow = [\"Bash(ls)\"]\n";
    put(h, ".codex/config.toml", codex);
    put(h, ".grok/config.toml", grok);

    setup(h, false);
    let r = setup(h, true);
    for a in &r.agents {
        assert!(
            a.state == State::Removed && !a.wrote.is_empty() && a.notes.is_empty(),
            "{a:?}"
        );
    }
    for (rel, v) in &json_files {
        assert_eq!(&json_at(h, rel), v, "{rel}");
    }
    assert_eq!(get(h, ".codex/config.toml").trim_end(), codex.trim_end());
    assert_eq!(get(h, ".grok/config.toml").trim_end(), grok.trim_end());
    // Our own Grok hook file is gone.
    assert!(!h.join(".grok/hooks/lightbulb.json").exists());
    // Nothing left to remove.
    assert!(setup(h, true).agents.iter().all(|a| a.wrote.is_empty()));
}

#[test]
fn any_lightbulb_mcp_entry_is_replaced_by_ours_and_a_foreign_one_survives_remove() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let other = json!({"mcpServers": {"lightbulb-network": {"command": APP, "args": ["serve"], "env": {"A": "1"}}}});
    let other_toml = format!(
        "[mcp_servers.lightbulb-network]\ncommand = \"{APP}\"\nargs = [\"serve\"]\nstartup_timeout_sec = 30\n"
    );
    let put_all = || {
        put(h, ".claude.json", &other.to_string());
        put(h, ".cursor/mcp.json", &other.to_string());
        put(h, ".codex/config.toml", &other_toml);
        put(h, ".grok/config.toml", &other_toml);
    };
    fs::create_dir(h.join(".claude")).unwrap();
    put_all();
    let r = run(h, Path::new(EXE), None, false);
    for name in ["claude", "codex", "cursor", "grok"] {
        assert_eq!(
            agent(&r, name).state,
            State::Connected("lightbulb-network".into()),
            "{name}"
        );
        assert_eq!(
            tools(h, name).as_deref(),
            Some("lightbulb-network"),
            "{name}"
        );
    }
    for rel in [".claude.json", ".cursor/mcp.json"] {
        let server = &json_at(h, rel)["mcpServers"]["lightbulb-network"];
        assert_eq!(server["command"], EXE, "{rel}");
        assert_eq!(server["args"], json!(["mcp"]), "{rel}");
        assert!(server.get("env").is_none(), "{rel}");
    }
    for rel in [".codex/config.toml", ".grok/config.toml"] {
        let doc = toml_at(h, rel);
        let server = &doc["mcp_servers"]["lightbulb-network"];
        assert_eq!(server["command"].as_str(), Some(EXE), "{rel}");
        assert!(server.get("startup_timeout_sec").is_none(), "{rel}");
    }
    // Removing takes out only ours.
    run(h, Path::new(EXE), None, true);
    assert!(json_at(h, ".claude.json").get("mcpServers").is_none());
    put_all();
    run(h, Path::new(EXE), None, true);
    assert_eq!(json_at(h, ".claude.json"), other);
    assert_eq!(json_at(h, ".cursor/mcp.json"), other);
    assert_eq!(get(h, ".codex/config.toml"), other_toml);
}

#[test]
fn setup_touches_only_installed_agents() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    fs::create_dir(h.join(".grok")).unwrap();
    let r = setup(h, false);
    let found: Vec<_> = r
        .agents
        .iter()
        .filter(|a| a.state != State::NotInstalled)
        .map(|a| a.name)
        .collect();
    assert_eq!(found, ["grok"]);
    for missing in [".claude", ".claude.json", ".codex", ".cursor"] {
        assert!(!h.join(missing).exists(), "{missing}");
    }
    assert_eq!(agent(&r, "claude").line(), "Claude Code: not installed");
}

#[test]
fn hook_command_quotes_the_executable() {
    let ctx = Ctx {
        home: Path::new("/"),
        exe: "/opt/it's/lightbulb",
        server: "lightbulb-network",
        remove: false,
    };
    let c = ctx.hook_command("grok", "stop");
    assert_eq!(c, r"'/opt/it'\''s/lightbulb' hook grok stop");
    assert!(ctx.is_ours(&c));
    assert!(!ctx.is_ours(&format!("'{APP}' local-session-hook grok stop")));
}

#[test]
fn backups_and_rewrites_keep_the_source_mode() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    put(h, ".claude/settings.json", "{}");
    put(h, ".claude.json", "{\"token\": \"secret\"}");
    for rel in [".claude/settings.json", ".claude.json"] {
        fs::set_permissions(h.join(rel), fs::Permissions::from_mode(0o600)).unwrap();
    }
    setup(h, false);
    for rel in [".claude/settings.json", ".claude.json"] {
        let p = h.join(rel);
        for f in [backup_path(&p), p] {
            let mode = fs::metadata(&f).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", f.display());
        }
    }
    // No temp files left behind.
    let left: Vec<_> = fs::read_dir(h.join(".claude"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("lightbulb-tmp"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_shared_group_keeps_the_other_hooks() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let mine = json!({"type": "command", "command": "echo mine"});
    let stale = json!({"type": "command", "command": cmd("claude", "stop"), "timeout": 99});
    put(
        h,
        ".claude/settings.json",
        &json!({"hooks": {"Stop": [{"hooks": [mine, stale]}]}}).to_string(),
    );
    setup(h, false);
    let ours = json!({"type": "command", "command": cmd("claude", "stop"), "timeout": 5});
    assert_eq!(
        json_at(h, ".claude/settings.json")["hooks"]["Stop"],
        json!([{"hooks": [mine, ours]}])
    );
    setup(h, true);
    assert_eq!(
        json_at(h, ".claude/settings.json"),
        json!({"hooks": {"Stop": [{"hooks": [mine]}]}})
    );
}

#[test]
fn files_setup_created_are_deleted_on_remove() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    fs::create_dir(h.join(".cursor")).unwrap();
    fs::create_dir(h.join(".grok")).unwrap();
    setup(h, false);
    assert!(h.join(".cursor/hooks.json").exists());
    assert!(h.join(".grok/hooks/lightbulb.json").exists());
    setup(h, true);
    assert!(!h.join(".cursor/hooks.json").exists());
    assert!(!h.join(".grok/hooks/lightbulb.json").exists());
}

#[test]
fn a_bad_server_name_writes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    fs::create_dir(h.join(".claude")).unwrap();
    let r = run(h, Path::new(EXE), Some("a:b"), false);
    assert!(agent(&r, "claude").wrote.is_empty());
    assert!(
        matches!(&agent(&r, "claude").state, State::Failed { why, .. } if why.contains("server name"))
    );
    assert!(!h.join(".claude.json").exists());
}

#[test]
fn a_non_default_server_name_is_passed_to_mcp() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    for dir in [".claude", ".codex", ".cursor", ".grok"] {
        fs::create_dir(h.join(dir)).unwrap();
    }
    run(h, Path::new(EXE), Some("work-tools"), false);
    let args = json!(["mcp", "--server-name", "work-tools"]);
    assert_eq!(
        json_at(h, ".claude.json")["mcpServers"]["work-tools"]["args"],
        args
    );
    assert_eq!(
        json_at(h, ".cursor/mcp.json")["mcpServers"]["work-tools"]["args"],
        args
    );
    for rel in [".codex/config.toml", ".grok/config.toml"] {
        let doc = toml_at(h, rel);
        let got: Vec<_> = doc["mcp_servers"]["work-tools"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(got, ["mcp", "--server-name", "work-tools"], "{rel}");
    }
}

#[test]
fn codex_says_it_trusted_the_hooks() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir(home.path().join(".codex")).unwrap();
    let r = setup(home.path(), false);
    assert_eq!(
        agent(&r, "codex").line(),
        "Codex: connected (hooks + tools); skill lightbulb-network. Its hook prompt was answered for you."
    );
    // Said once: a second run has nothing new to trust.
    assert_eq!(
        agent(&setup(home.path(), false), "codex").line(),
        "Codex: connected (hooks + tools)"
    );
}

#[test]
fn a_codex_hook_in_a_shared_group_says_codex_will_ask() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let other = json!({"type": "command", "command": "echo other"});
    let stale = json!({"type": "command", "command": cmd("codex", "stop")});
    put(
        h,
        ".codex/hooks.json",
        &json!({"hooks": {"Stop": [{"hooks": [other, stale]}]}}).to_string(),
    );
    let r = setup(h, false);
    let codex = agent(&r, "codex");
    assert!(
        codex.notes.iter().any(|n| n
            == "Codex: the Stop hook shares a group with another tool's; Codex will ask you to trust it once (/hooks)"),
        "{:?}",
        codex.notes
    );
    // Only the shared group goes untrusted.
    let config = get(h, ".codex/config.toml");
    assert!(!config.contains(":stop:"), "{config}");
    assert!(config.contains(":session_start:0:0"), "{config}");
}

#[test]
fn the_closing_words_name_what_is_connected_and_the_command_to_type() {
    assert_eq!(
        summary(&["Claude Code", "Codex", "Cursor", "Grok CLI"], "lightbulb"),
        "Lightbulb is on.
  Connected: Claude Code, Codex, Cursor, Grok CLI

Try it: open Claude Code in any folder and ask \"list my other sessions\".
Add your other computer or a teammate: lightbulb invite
Check everything: lightbulb doctor
Add an always-on AI teammate: https://thelightbulb.company"
    );
    assert!(summary(&["Codex"], "lightbulb").contains("open Codex in any folder"));
}

#[test]
fn a_file_it_cannot_read_is_a_failure_not_a_name_clash() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    // The tools file is broken: the hooks still go in.
    fs::create_dir(h.join(".claude")).unwrap();
    put(h, ".claude.json", "{not json");
    // The hooks file is broken: nothing goes in.
    put(h, ".cursor/hooks.json", "[]");
    let r = run(h, Path::new(EXE), None, false);
    let (claude, cursor) = (agent(&r, "claude"), agent(&r, "cursor"));
    assert!(matches!(claude.state, State::Failed { hooks: true, .. }));
    assert!(matches!(cursor.state, State::Failed { hooks: false, .. }));
    let line = claude.line();
    assert!(
        line.starts_with("Claude Code: hooks added, but not its tools — "),
        "{line}"
    );
    assert!(line.contains(".claude.json"), "{line}");
    let line = cursor.line();
    assert!(line.starts_with("Cursor: not connected — "), "{line}");
    assert!(!line.contains("tool server name"), "{line}");
}

#[test]
fn an_http_tool_server_under_the_name_is_replaced() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    fs::create_dir(h.join(".claude")).unwrap();
    let other =
        json!({"mcpServers": {"lightbulb-network": {"type": "http", "url": "https://x/mcp"}}});
    put(h, ".claude.json", &other.to_string());
    let r = run(h, Path::new(EXE), None, false);
    assert_eq!(
        agent(&r, "claude").state,
        State::Connected("lightbulb-network".into())
    );
    assert_eq!(
        json_at(h, ".claude.json")["mcpServers"]["lightbulb-network"],
        json!({"command": EXE, "args": ["mcp"]})
    );
}

#[test]
fn the_skill_is_installed_for_claude_and_codex_and_removed_with_setup() {
    let d = tempfile::tempdir().unwrap();
    let h = d.path();
    for dir in [".claude", ".codex", ".cursor", ".grok"] {
        fs::create_dir_all(h.join(dir)).unwrap();
    }
    setup(h, false);
    let rel = ".claude/skills/lightbulb-network/SKILL.md";
    let skill = get(h, rel);
    assert!(skill.starts_with("---\nname: lightbulb-network\ndescription: "));
    assert_eq!(get(h, ".codex/skills/lightbulb-network/SKILL.md"), skill);
    assert!(!h.join(".cursor/skills").exists() && !h.join(".grok/skills").exists());
    // A second run changes nothing; another tool's skills are left alone.
    put(h, ".claude/skills/other/SKILL.md", "mine");
    assert!(setup(h, false).agents[0].notes.is_empty());
    setup(h, true);
    assert!(!h.join(".claude/skills/lightbulb-network").exists());
    assert!(!h.join(".codex/skills/lightbulb-network").exists());
    assert_eq!(get(h, ".claude/skills/other/SKILL.md"), "mine");
}

#[test]
fn our_old_lightbulb_entry_moves_to_the_new_name_and_the_apps_stays() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    fs::create_dir(h.join(".claude")).unwrap();
    // Ours under the old name, beside nothing; then the desktop app's under the old name.
    for old_cmd in [EXE, APP] {
        put(
            h,
            ".claude.json",
            &json!({"mcpServers": {"lightbulb": {"command": old_cmd, "args": ["x"]}}}).to_string(),
        );
        put(
            h,
            ".codex/config.toml",
            &format!("[mcp_servers.lightbulb]\ncommand = \"{old_cmd}\"\nargs = [\"x\"]\n"),
        );
        run(h, Path::new(EXE), None, false);
        let claude = json_at(h, ".claude.json");
        let codex = toml_at(h, ".codex/config.toml");
        assert_eq!(claude["mcpServers"]["lightbulb-network"]["command"], EXE);
        assert_eq!(
            codex["mcp_servers"]["lightbulb-network"]["command"].as_str(),
            Some(EXE)
        );
        let kept = old_cmd == APP;
        assert_eq!(
            !claude["mcpServers"]["lightbulb"].is_null(),
            kept,
            "{old_cmd}"
        );
        assert_eq!(
            codex["mcp_servers"].get("lightbulb").is_some(),
            kept,
            "{old_cmd}"
        );
        if kept {
            assert_eq!(claude["mcpServers"]["lightbulb"]["command"], APP);
        }
    }
}
