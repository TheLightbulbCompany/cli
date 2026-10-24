use clap::{Parser, Subcommand};
use lightbulb::daemon::net_ops::Invite;
use lightbulb::{cli, daemon, hook, mcp, paths, setup};
use std::ffi::OsString;
use std::io::{Read, Write};

/// The top-level help: the person's commands in groups (`hook`, `mcp` and `daemon` are the
/// agents' and launchd's, so not listed; they are still accepted).
const HELP: &str = "\
Lightbulb lets your coding agents message each other: on this computer, on your other
computers and on your teammates'.

Usage: lightbulb <command> [options]

Getting started:
  setup     Connect Claude Code, Codex, Cursor and Grok CLI here and start the background service
  doctor    Check that everything is connected, and say how to fix what is not
  invite    Print the join line that adds your other computer or a teammate
  join      Join the team a join line is for: join '<ticket>'
  signin    Sign in to Lightbulb, so teammates see your verified name
  update    Install the latest release (or --version vX.Y.Z) and restart the background service

Everyday:
  who       List the sessions here (who --all: your teammates' too)
  send      Send a message: send codex-1 \"hi\", or to a name: send demo2/inbox \"hi\"
  listen    Print the messages sent to a name: listen --as inbox
  receipt   Whether a message you sent arrived: receipt <id>
  held      Join requests and messages waiting for your Allow
  approve   Allow one: approve J1, approve H1
  decline   Decline one: decline H1

Team:
  team      The team and its computers
  admin     Make a member an admin (or with --founder, the founder)
  remove    Remove a member
  leave     Leave the team
  policy    Who may message your sessions without asking
  folder    Show or hide the sessions in a folder from your teammates: folder <path> on|off

Run lightbulb <command> --help for its options; lightbulb --version for the version.
";

#[derive(Parser)]
#[command(name = "lightbulb", bin_name = "lightbulb", version, help_template = HELP)]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
        /// Replace the reusable invite; the old one stops working.
        #[arg(long, conflicts_with_all = ["once", "open"])]
        new: bool,
        /// An invite for one join, valid 24 hours.
        #[arg(long, conflicts_with = "open")]
        once: bool,
        /// An invite whose joins need no Allow (founder and admins).
        #[arg(long)]
        open: bool,
    },
    /// Show the team and its members.
    Team,
    /// Make a member an admin (founder only), or with --founder the founder in your place.
    Admin {
        name: String,
        #[arg(long)]
        founder: bool,
    },
    /// Remove a member (founder and admins); the invite is replaced.
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
        /// `cloud`: join as a workspace member (tests and ops; needs `--proof`).
        #[arg(long, hide = true, default_value = "computer", value_parser = ["computer", "cloud"])]
        kind: String,
        /// The workspace member's `kind=workspace` proof.
        #[arg(long, hide = true)]
        proof: Option<String>,
    },
    /// Sign in to Lightbulb in your browser, so teammates see your verified name.
    Signin,
    /// Install the latest release (or `--version`) over this one, then run its `setup`.
    Update {
        /// The release to install, like v0.2.2 (default: the latest).
        #[arg(long)]
        version: Option<String>,
    },
    /// Allow a join request (`J<n>`) or a held message (`H<n>`).
    Approve {
        request: String,
        /// For a held message from a cloud member: always allow that workspace's agents.
        #[arg(long)]
        always: bool,
    },
    /// Decline a join request (`J<n>`) or a held message (`H<n>`).
    Decline { request: String },
    /// The join requests and held messages waiting for your Allow.
    Held,
    /// Who may message your sessions; with a person and a value, sets it.
    Policy {
        person: Option<String>,
        /// accept, hold or refuse.
        value: Option<String>,
    },
    /// Show (`on`) or hide (`off`) the sessions under a folder from your teammates.
    Folder {
        path: String,
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// The relays this computer uses (with a URL, sets yours; from the founder or an admin,
    /// the team's too). Relays carry encrypted traffic only, when no direct path exists.
    Relay {
        url: Option<String>,
        /// Clear the relay you set.
        #[arg(long, conflicts_with = "url")]
        clear: bool,
        /// Only this computer, not the team (founder and admins set the team's by default).
        #[arg(long)]
        local: bool,
    },
    #[command(external_subcommand)]
    External(Vec<OsString>),
    /// Run by an agent's hook config: `hook <claude|codex|cursor|grok> <session-start|user-prompt-submit|post-tool-use|stop|session-end>`.
    ///
    /// Always exits 0; errors go to stderr.
    #[command(hide = true)]
    Hook { agent: String, event: String },
    /// Run the stdio MCP server an agent talks to (find_sessions, send_message, ...).
    #[command(hide = true)]
    Mcp {
        /// The name this server advertises: the one its MCP entry was installed under.
        #[arg(long, default_value = setup::DEFAULT_SERVER)]
        server_name: String,
    },
    /// Wire the installed agents (Claude Code, Codex, Cursor, Grok CLI) to this engine:
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
        Command::Invite { new, once, open } => exit(cli::invite(match (new, once, open) {
            (true, ..) => Invite::New,
            (_, true, _) => Invite::Once,
            (.., true) => Invite::Open,
            _ => Invite::Reusable,
        })?),
        Command::Team => exit(cli::team()?),
        Command::Admin { name, founder } => exit(cli::admin(&name, founder)?),
        Command::Remove { name } => exit(cli::remove(&name)?),
        Command::Leave => exit(cli::leave()?),
        Command::Join {
            link,
            name,
            kind,
            proof,
        } => exit(cli::join(&link, name, &kind, proof)?),
        Command::Signin => exit(cli::signin()?),
        Command::Update { version } => exit(cli::update(version)?),
        Command::Approve { request, always } => exit(cli::answer(&request, true, always)?),
        Command::Decline { request } => exit(cli::answer(&request, false, false)?),
        Command::Held => exit(cli::held()?),
        Command::Policy { person, value } => exit(cli::policy(person, value)?),
        Command::Folder { path, state } => exit(cli::folder(&path, state == "on")?),
        Command::Relay { url, clear, local } => {
            exit(cli::relay(clear.then(String::new).or(url), local)?)
        }
        Command::External(args) => {
            let verb = args
                .first()
                .map(|a| a.to_string_lossy())
                .unwrap_or_default();
            eprintln!("Unknown command: {verb}. Run lightbulb --help.");
            exit(2)
        }
        Command::Mcp { server_name } => mcp::run(server_name)?,
        Command::Setup {
            remove,
            executable,
            server_name,
            verbose,
        } => exit(cli::setup(remove, executable, server_name, verbose)?),
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
