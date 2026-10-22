//! Every test runs on a temp home, never the real one.

use super::*;
use serde_json::json;
use std::path::Path;

const EXE: &str = "/opt/lb/bin/lightbulb-network";
const APP: &str = "/Applications/Lightbulb.app/Contents/MacOS/lightbulb-desktop";
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
    run(home, Path::new(EXE), "lightbulb", remove)
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
    assert!(claude.found);
    assert_eq!(
        claude.wrote,
        [h.join(".claude/settings.json"), h.join(".claude.json")]
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
        c["mcpServers"]["lightbulb"],
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
        [h.join(".codex/hooks.json"), h.join(".codex/config.toml")]
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
    let server = &toml["mcp_servers"]["lightbulb"];
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
        m["mcpServers"]["lightbulb"],
        json!({"type": "stdio", "command": EXE, "args": ["mcp"]})
    );
    assert_eq!(m["mcpServers"]["filesystem"], json!({"command": "npx"}));
    assert_eq!(
        json_at(h, ".cursor/cli-config.json")["permissions"]["allow"],
        json!(["Shell(ls)", "Mcp(lightbulb:*)"])
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
            h.join(".grok/hooks/lightbulb-network.json"),
            h.join(".grok/config.toml")
        ]
    );
    let hooks = json_at(h, ".grok/hooks/lightbulb-network.json");
    for (event, arg) in EVENTS {
        let timeout = if event == "SessionEnd" { 3 } else { 10 };
        let mut ours = json!({"hooks": [{"type": "command", "command": cmd("grok", arg), "timeout": timeout}]});
        if event == "PostToolUse" {
            ours["matcher"] = json!("*");
        }
        assert_eq!(hooks["hooks"][event], json!([ours]), "{event}");
    }
    let toml = toml_at(h, ".grok/config.toml");
    let server = &toml["mcp_servers"]["lightbulb"];
    assert_eq!(server["command"].as_str(), Some(EXE));
    assert_eq!(server["enabled"].as_bool(), Some(true));
    let allow: Vec<_> = toml["permission"]["allow"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(allow, ["Bash(ls)", "MCPTool(lightbulb__*)"]);
    assert_eq!(toml["cli"]["installer"].as_str(), Some("npm"));

    assert!(agent(&setup(h, false), "grok").wrote.is_empty());
    backed_up_once(h, ".grok/config.toml", config);
}

#[test]
fn remove_takes_only_our_entries() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let app_server = json!({"command": APP, "args": ["session-mcp"]});
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
            a.found && !a.wrote.is_empty() && a.notes.is_empty(),
            "{a:?}"
        );
    }
    for (rel, v) in &json_files {
        assert_eq!(&json_at(h, rel), v, "{rel}");
    }
    assert_eq!(get(h, ".codex/config.toml").trim_end(), codex.trim_end());
    assert_eq!(get(h, ".grok/config.toml").trim_end(), grok.trim_end());
    // Our own Grok hook file is gone.
    assert!(!h.join(".grok/hooks/lightbulb-network.json").exists());
    // Nothing left to remove.
    assert!(setup(h, true).agents.iter().all(|a| a.wrote.is_empty()));
}

#[test]
fn a_foreign_lightbulb_mcp_entry_is_left_alone_and_gets_no_allow_rule() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let app = json!({"mcpServers": {"lightbulb": {"type": "stdio", "command": APP, "args": ["session-mcp"]}}});
    let app_toml =
        format!("[mcp_servers.lightbulb]\ncommand = \"{APP}\"\nargs = [\"session-mcp\"]\n");
    fs::create_dir(h.join(".claude")).unwrap();
    put(h, ".claude.json", &app.to_string());
    put(h, ".cursor/mcp.json", &app.to_string());
    put(
        h,
        ".cursor/cli-config.json",
        &json!({"permissions": {"allow": []}}).to_string(),
    );
    put(h, ".codex/config.toml", &app_toml);
    let grok = format!("{app_toml}\n[permission]\nallow = [\"MCPTool(lightbulb__*)\"]\n");
    put(h, ".grok/config.toml", &grok);

    for remove in [false, true] {
        let r = setup(h, remove);
        assert_eq!(json_at(h, ".claude.json"), app);
        assert_eq!(json_at(h, ".cursor/mcp.json"), app);
        // Not added for the foreign server, and the app's own rule is not ours to take.
        assert_eq!(
            json_at(h, ".cursor/cli-config.json"),
            json!({"permissions": {"allow": []}})
        );
        assert_eq!(get(h, ".grok/config.toml"), grok);
        if !remove {
            assert!(get(h, ".codex/config.toml").starts_with(&app_toml));
            for name in ["claude", "codex", "cursor", "grok"] {
                assert!(
                    agent(&r, name).notes.iter().any(|n| n.contains("not ours")),
                    "{name}"
                );
            }
        }
    }
    assert_eq!(get(h, ".codex/config.toml").trim_end(), app_toml.trim_end());
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
        .filter(|a| a.found)
        .map(|a| a.name)
        .collect();
    assert_eq!(found, ["grok"]);
    for missing in [".claude", ".claude.json", ".codex", ".cursor"] {
        assert!(!h.join(missing).exists(), "{missing}");
    }
    assert_eq!(agent(&r, "claude").line(), "claude: not installed");
}

#[test]
fn hook_command_quotes_the_executable() {
    let ctx = Ctx {
        home: Path::new("/"),
        exe: "/opt/it's/lightbulb-network",
        server: "lightbulb",
        remove: false,
    };
    let c = ctx.hook_command("grok", "stop");
    assert_eq!(c, r"'/opt/it'\''s/lightbulb-network' hook grok stop");
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
    assert!(h.join(".grok/hooks/lightbulb-network.json").exists());
    setup(h, true);
    assert!(!h.join(".cursor/hooks.json").exists());
    assert!(!h.join(".grok/hooks/lightbulb-network.json").exists());
}

#[test]
fn a_grok_lightbulb_json_of_someone_else_is_never_touched() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let app = json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "app"}]}]}})
        .to_string();
    put(h, ".grok/hooks/lightbulb.json", &app);
    setup(h, false);
    setup(h, true);
    assert_eq!(get(h, ".grok/hooks/lightbulb.json"), app);
    assert!(!backup_path(&h.join(".grok/hooks/lightbulb.json")).exists());
}

#[test]
fn a_bad_server_name_writes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    fs::create_dir(h.join(".claude")).unwrap();
    let r = run(h, Path::new(EXE), "a:b", false);
    assert!(agent(&r, "claude").wrote.is_empty());
    assert!(agent(&r, "claude").notes[0].contains("server name"));
    assert!(!h.join(".claude.json").exists());
}

#[test]
fn codex_says_it_trusted_the_hooks() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir(home.path().join(".codex")).unwrap();
    let r = setup(home.path(), false);
    assert!(agent(&r, "codex")
        .line()
        .contains("Codex hooks trusted on your behalf"));
    assert!(agent(&setup(home.path(), false), "codex").notes.is_empty());
}
