use clap::{Parser, Subcommand};
use lightbulb::daemon::net_ops::Invite;
use lightbulb::{cli, daemon, hook, mcp, member, paths, setup};
use std::ffi::OsString;
use std::io::{Read, Write};

/// The top-level help: the person's commands in groups (`hook`, `mcp` and `daemon` are the
/// agents' and launchd's, so not listed; they are still accepted).
const HELP: &str = "\
Lightbulb lets your coding agents message each other: on this computer, on your other
computers and on your teammates'.

Usage: lightbulb <command> [options]

Getting started:
  setup     Connect Claude Code, Codex, Cursor, Grok CLI and OpenCode here and start the background service
  doctor    Check that everything is connected, and say how to fix what is not
  invite    Print the join line that adds your other computer or a teammate
            (invite --agents: add your Lightbulb workspace's agents, as a workspace admin)
  join      Join the team a join line is for: join '<ticket>'
  update    Install the latest release (or --version vX.Y.Z) and restart the background service

Everyday:
  who       List the sessions here (who --all: your teammates' too)
  send      Send a message: send codex-1 \"hi\", or to a name: send demo2/inbox \"hi\"
  listen    Print the messages sent to a name: listen --as inbox
  receipt   Whether a message you sent arrived: receipt <id>
  held      Permission prompts waiting for your answer
  approve   Allow one: approve H1
  decline   Decline one: decline H1
  config    Set a true/false or number setting: config set network_workers true

Workers:
  workers   Let teammates start Claude Code or Codex workers here: workers on|off, workers show <name>,
            workers stop <name>, workers policy, workers clean

Team:
  team      The team and its computers
  remove    Remove a member
  leave     Leave the team
  login     Sign this computer in to Lightbulb, so your workspace's agents answer it
  logout    Sign this computer out
  folder    Show or hide the sessions in a folder from your teammates: folder <path> on|off
  relay     The relays used when no direct path exists; relay <url> sets yours
  telemetry Show or set anonymous usage counts: telemetry on|off|status

Apps you build:
  app       Build, check, preview, publish and undo apps: app new|up|publish|rollback|status|destroy|check|evidence|report-broken-tool

With the Lightbulb app on this Mac: claude, codex, continue '<link>', share on|off and pair

Run lightbulb <command> --help for its options; lightbulb --version for the version.
";

#[derive(Parser)]
#[command(name = "lightbulb", bin_name = "lightbulb", version, help_template = HELP)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum MemoryCommand {
    /// What is remembered now: scope, who, status, subject and value.
    List {
        /// Only what is shared with the team.
        #[arg(long, conflicts_with = "me")]
        team: bool,
        /// Only what is yours alone.
        #[arg(long)]
        me: bool,
        /// Only what was remembered in this repository (its git origin).
        #[arg(long)]
        repo: Option<String>,
    },
    /// One entry in full: its value, who wrote it, where from, and what it replaced.
    Show { id: String },
    /// Delete an entry on every computer that holds it.
    Forget { id: String },
    /// Copy one of your own entries to the team; the original stays yours.
    Share { id: String },
    /// Print every entry this computer holds, one JSON object per line (`--json`: one array).
    Export {
        #[arg(long)]
        json: bool,
    },
    /// Print the memory block your last session was handed at its start.
    Last,
    /// Lessons Lightbulb's evaluator suggests from sessions you uploaded, waiting for you.
    Drafts,
    /// Keep a suggested lesson (`--team`: for the whole workspace).
    Approve {
        id: String,
        #[arg(long)]
        team: bool,
    },
    /// Throw a suggested lesson away.
    Dismiss { id: String },
    /// Let agents write team memory (`on`) or not (`off`) from this computer.
    Team {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
}

#[derive(Subcommand)]
enum SessionsCommand {
    /// Your coding sessions captured on this computer: agent, repo, tool calls and tokens.
    List,
    /// One session with its tool calls (a tool and a short summary, never file contents).
    Show { id: String },
    /// Delete a session and its tool calls.
    Forget { id: String },
    /// Print every captured session with its tool calls as JSON.
    Export,
    /// Keep your prompts and the agent's replies on this computer (on, the default) or not (off). No state: say which.
    Turns {
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
    },
    /// Choose what is sent to Lightbulb: off (the default), metadata or full. No mode: say which.
    Upload {
        #[arg(value_parser = ["off", "metadata", "full"])]
        mode: Option<String>,
    },
}

#[derive(Subcommand)]
enum Command {
    /// Print the version.
    Version,
    /// Run the per-computer daemon in the foreground, or install/remove its launchd job.
    #[command(hide = true)]
    Daemon {
        /// Write the launchd job (and the `lightbulb` alias) and load it.
        #[arg(long, conflicts_with = "uninstall")]
        install: bool,
        /// Stop the daemon and remove the launchd job; state is kept.
        #[arg(long)]
        uninstall: bool,
    },
    /// List the coding sessions on this computer (with `--all`, then every teammate's).
    Who {
        /// Print the raw session list as JSON.
        #[arg(long)]
        json: bool,
        /// Also list the sessions on teammates' computers.
        #[arg(long)]
        all: bool,
    },
    /// Send text to a session (`session_id`), a name address (`<member>/<name>`), or, from a
    /// person's terminal, a group (`*`, `<member>/*`).
    Send {
        target: String,
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        /// Send as this session (a person speaking for it) instead of the terminal.
        #[arg(long)]
        from: Option<String>,
        /// The request_id of the message this answers.
        #[arg(long)]
        reply_to: Option<String>,
        /// Use this request_id instead of a new one (to retry a send).
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Print messages addressed to the name `<member>/<name>` as they arrive (Ctrl-C to stop).
    Listen {
        #[arg(long = "as")]
        name: String,
        /// Return after the first message.
        #[arg(long)]
        once: bool,
    },
    /// Print the receipt of a message sent from this terminal (or `--from` another address).
    Receipt {
        request_id: String,
        #[arg(long)]
        from: Option<String>,
        /// Print the receipt as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Check the daemon, state permissions, agent wiring, alias and launchd job.
    Doctor,
    /// Print this computer's join line (the reusable invite, unless a flag says otherwise).
    Invite {
        /// A new invite; every earlier one from this computer stops working.
        #[arg(long, conflicts_with = "once")]
        new: bool,
        /// An invite for one join, valid 24 hours.
        #[arg(long)]
        once: bool,
        /// Add your Lightbulb workspace's agents to this network (a workspace admin, signed in
        /// with `lightbulb login`): confirm in the browser, and the workspace joins.
        #[arg(long, conflicts_with_all = ["new", "once"])]
        agents: bool,
    },
    /// Sign this computer in to Lightbulb (confirm in the browser): your workspace's agents
    /// answer the computers of its signed-in members.
    Login,
    /// Sign this computer out of Lightbulb: workspaces stop answering it.
    Logout,
    /// Show the team and its members.
    Team,
    /// Remove a member; the invite is replaced.
    Remove { name: String },
    /// Leave the team; this computer is on its own network again.
    Leave,
    /// Join the network a join line is for (this computer must be on its own).
    Join {
        /// The line your inviter sent (the long code after join, or the whole link).
        #[arg(value_name = "TICKET")]
        link: String,
        /// The name you go by in the network (default: your user name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Verified names are not available yet (they come with the issuer's real keys).
    #[command(hide = true)]
    Signin,
    /// Install the latest release (or `--version`) over this one, then run its `setup`.
    Update {
        /// The release to install, like v0.2.2 (default: the latest).
        #[arg(long)]
        version: Option<String>,
        /// Turn the daily automatic update `on` or `off` (and install nothing).
        #[arg(long, value_parser = ["on", "off"], conflicts_with = "version")]
        auto: Option<String>,
    },
    /// Allow a permission prompt or a worker start (`H<n>`); `--always` also allows that member's
    /// workers from now on.
    Approve {
        request: String,
        #[arg(long)]
        always: bool,
    },
    /// Decline a permission prompt (`H<n>`).
    Decline { request: String },
    /// The permission prompts waiting for your answer.
    Held,
    /// Set a true/false or number setting: config set <key> <value>.
    Config {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Workers teammates start here: list, show, stop, on, off, policy, clean.
    Workers {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Anonymous usage counts: `on`, `off`, or (no argument) their state.
    Telemetry {
        #[arg(value_parser = ["on", "off", "status"])]
        state: Option<String>,
    },
    /// What your agents and your team remember: list, show, forget, share, export.
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// The sessions captured on this computer: list, show, forget, export, turns.
    Sessions {
        #[command(subcommand)]
        command: SessionsCommand,
    },
    /// Show (`on`) or hide (`off`) the sessions under a folder from your teammates.
    Folder {
        path: String,
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// The relays this computer uses (with a URL, sets yours and the team's too). Relays
    /// carry encrypted traffic only, when no direct path exists.
    Relay {
        url: Option<String>,
        /// Clear the relay you set.
        #[arg(long, conflicts_with = "url")]
        clear: bool,
        /// Only this computer, not the team (otherwise the team's too).
        #[arg(long)]
        local: bool,
    },
    /// Apps you build: new, up, publish, rollback, status, destroy, check, evidence.
    App {
        #[command(subcommand)]
        command: lightbulb::apps::AppCommand,
        /// Print JSON (the default when stdout is not a terminal).
        #[arg(long, global = true)]
        json: bool,
    },
    #[command(external_subcommand)]
    External(Vec<OsString>),
    /// Run by an agent's hook config: `hook <claude|codex|cursor|grok|opencode> <session-start|user-prompt-submit|post-tool-use|stop|session-end>`.
    ///
    /// Always exits 0; errors go to stderr.
    #[command(hide = true)]
    Hook { agent: String, event: String },
    /// Run the engine as a cloud workspace's sidecar, driven over `--socket` by the Lightbulb
    /// bridge; the socket's token is the first line of stdin.
    #[command(hide = true)]
    Member {
        /// The instance's state dir.
        #[arg(long)]
        home: std::path::PathBuf,
        /// Where the final copy of state.db goes at stop (outside --home).
        #[arg(long)]
        snapshot: std::path::PathBuf,
        #[arg(long)]
        socket: std::path::PathBuf,
        /// The workspace's display name.
        #[arg(long)]
        name: String,
        #[arg(long)]
        relay: Option<String>,
        /// The sessions file: {"workspace", "sessions": [{"name", "agent", "title"?}]}.
        #[arg(long)]
        sessions: std::path::PathBuf,
    },
    /// Run the stdio MCP server an agent talks to (find_sessions, send_message, ...).
    #[command(hide = true)]
    Mcp {
        /// The name this server advertises: the one its MCP entry was installed under.
        #[arg(long, default_value = setup::DEFAULT_SERVER)]
        server_name: String,
        /// Print the tools' spec (tools.json) and exit.
        #[arg(long)]
        tools_json: bool,
    },
    /// Wire the installed agents (Claude Code, Codex, Cursor, Grok CLI, OpenCode) to this engine:
    /// their hooks and MCP server entry. Each file is backed up once as <file>.lightbulb-backup.
    Setup {
        /// Take out only what setup wrote; backups are left as they are.
        #[arg(long)]
        remove: bool,
        /// The engine binary the hooks, MCP entries and launchd job run (default: the canonical
        /// copy, ~/.lightbulb/connect/bin/lightbulb, which setup installs from this one).
        #[arg(long)]
        executable: Option<std::path::PathBuf>,
        /// The MCP server entry's name, also used in Cursor's and Grok's allow rules (default:
        /// lightbulb).
        #[arg(long)]
        server_name: Option<String>,
        /// Also list the files each agent's setup changed.
        #[arg(long)]
        verbose: bool,
        /// Skip downloading the local memory model (about 31 MB); memory search stays
        /// keyword-only.
        #[arg(long)]
        no_model: bool,
        /// (The automatic setup after an update: keeps the person's model choice.)
        #[arg(long, hide = true)]
        refresh: bool,
    },
}

/// Exits with `code` unless it is 0.
fn exit(code: i32) {
    if code != 0 {
        std::process::exit(code);
    }
}

fn main() {
    // An error is a sentence for the person, never Rust's debug print.
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn run() -> lightbulb::Result<()> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // A hook must never fail its agent, even on bad arguments.
        Err(e) if std::env::args().nth(1).as_deref() == Some("hook") => {
            let _ = e.print();
            return Ok(());
        }
        Err(e) => e.exit(),
    };
    match cli.command {
        Command::Version => println!("lightbulb {}", env!("CARGO_PKG_VERSION")),
        Command::Daemon { install: true, .. } => exit(cli::daemon_install()?),
        Command::Daemon {
            uninstall: true, ..
        } => exit(cli::daemon_uninstall()?),
        Command::Daemon { .. } => {
            tokio::runtime::Runtime::new()?.block_on(daemon::run(&paths::state_dir()))?
        }
        Command::Who { json, all } => exit(cli::who(json, all)?),
        Command::Send {
            target,
            text,
            from,
            reply_to,
            request_id,
        } => exit(cli::send(
            &target,
            &text.join(" "),
            from,
            reply_to,
            request_id,
        )?),
        Command::Listen { name, once } => exit(cli::listen(&name, once)?),
        Command::Receipt {
            request_id,
            from,
            json,
        } => exit(cli::receipt(&request_id, from, json)?),
        Command::Doctor => exit(cli::doctor()?),
        Command::Invite { agents: true, .. } => exit(cli::invite_agents()?),
        Command::Login => exit(lightbulb::login::login()?),
        Command::Logout => exit(lightbulb::login::logout()?),
        Command::Invite { new, once, .. } => exit(cli::invite(match (new, once) {
            (true, _) => Invite::New,
            (_, true) => Invite::Once,
            _ => Invite::Reusable,
        })?),
        Command::Team => exit(cli::team()?),
        Command::Remove { name } => exit(cli::remove(&name)?),
        Command::Leave => exit(cli::leave()?),
        Command::Join { link, name } => exit(cli::join(&link, name)?),
        Command::Signin => exit(cli::signin()?),
        Command::Update { auto: Some(a), .. } => {
            let dir = lightbulb::paths::state_dir();
            lightbulb::config::Config::set(
                &dir,
                "auto_update",
                if a == "on" { "true" } else { "false" },
            )?;
            println!("Automatic updates {a}.");
        }
        Command::Update { version, .. } => exit(cli::update(version)?),
        Command::Approve { request, always } => exit(cli::answer(&request, true, always)?),
        Command::Decline { request } => exit(cli::answer(&request, false, false)?),
        Command::Held => exit(cli::held()?),
        Command::Workers { args } => exit(cli::workers(&args)?),
        Command::Config { args } => exit(cli::config(&args)?),
        Command::Telemetry { state } => exit(lightbulb::telemetry::command(
            &lightbulb::paths::state_dir(),
            state.as_deref().filter(|s| *s != "status"),
        )?),
        Command::Memory { command } => exit(cli::memory(match command {
            MemoryCommand::List { team, me, repo } => cli::Memory::List { team, me, repo },
            MemoryCommand::Show { id } => cli::Memory::Show(id),
            MemoryCommand::Forget { id } => cli::Memory::Forget(id),
            MemoryCommand::Share { id } => cli::Memory::Share(id),
            MemoryCommand::Export { json } => cli::Memory::Export(json),
            MemoryCommand::Last => cli::Memory::Last,
            MemoryCommand::Drafts => cli::Memory::Drafts,
            MemoryCommand::Approve { id, team } => cli::Memory::Approve(id, team),
            MemoryCommand::Dismiss { id } => cli::Memory::Dismiss(id),
            MemoryCommand::Team { state } => cli::Memory::Team(state == "on"),
        })?),
        Command::Sessions { command } => exit(cli::sessions(match command {
            SessionsCommand::List => cli::Sessions::List,
            SessionsCommand::Show { id } => cli::Sessions::Show(id),
            SessionsCommand::Forget { id } => cli::Sessions::Forget(id),
            SessionsCommand::Export => cli::Sessions::Export,
            SessionsCommand::Turns { state } => cli::Sessions::Turns(state.map(|s| s == "on")),
            SessionsCommand::Upload { mode } => cli::Sessions::Upload(mode),
        })?),
        Command::Folder { path, state } => exit(cli::folder(&path, state == "on")?),
        Command::Relay { url, clear, local } => {
            exit(cli::relay(clear.then(String::new).or(url), local)?)
        }
        Command::App { command, json } => {
            use std::io::IsTerminal;
            let json = json || !std::io::stdout().is_terminal();
            // Every app failure has the `{code, message, next}` shape, even an unexpected one.
            exit(lightbulb::apps::run(command, json).unwrap_or_else(|e| {
                lightbulb::apps::fail(json, "failed", &e.to_string(), "try again")
            }))
        }
        Command::External(args) if cli::APP_WORDS.iter().any(|w| args[0] == *w) => {
            exit(cli::app_command(&args)?)
        }
        Command::External(args) => {
            let verb = args
                .first()
                .map(|a| a.to_string_lossy())
                .unwrap_or_default();
            eprintln!("Unknown command: {verb}. Run lightbulb --help.");
            exit(2)
        }
        Command::Mcp {
            tools_json: true, ..
        } => print!("{}", mcp::TOOLS),
        Command::Mcp { server_name, .. } => mcp::run(server_name)?,
        Command::Member {
            home,
            snapshot,
            socket,
            name,
            relay,
            sessions,
        } => {
            let mut token = String::new();
            std::io::stdin().read_line(&mut token)?;
            let token = token.trim_end_matches(['\n', '\r']).to_string();
            let opts = member::MemberOpts {
                home,
                snapshot,
                socket,
                name,
                relay,
                sessions,
            };
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(member::run(opts, token))?;
            rt.shutdown_timeout(std::time::Duration::from_secs(1));
        }
        Command::Setup {
            remove,
            executable,
            server_name,
            verbose,
            no_model,
            refresh,
        } => exit(cli::setup(
            remove,
            executable,
            server_name,
            verbose,
            (!refresh).then_some(!no_model),
        )?),
        Command::Hook { agent, event } => {
            let mut stdin = String::new();
            let ran = std::io::stdin()
                .read_to_string(&mut stdin)
                .map_err(Into::into)
                .and_then(|_| hook::run(&agent, &event, &stdin));
            match ran {
                // Write errors (a closed pipe) are ignored: the hook still exits 0.
                Ok(Some(out)) => {
                    let _ = writeln!(std::io::stdout(), "{out}");
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = writeln!(std::io::stderr(), "lightbulb hook {agent} {event}: {e}");
                }
            }
        }
    }
    Ok(())
}
