use clap::{Parser, Subcommand};
use lightbulb_connect::daemon::net_ops::Invite;
use lightbulb_connect::{cli, daemon, hook, mcp, paths, setup};
use std::ffi::OsString;
use std::io::{Read, Write};

#[derive(Parser)]
#[command(name = "lightbulb-connect", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the version.
    Version,
    /// Run the per-computer daemon in the foreground, or install/remove its launchd job.
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
    },
    /// Check the daemon, state permissions, agent wiring, alias and launchd job.
    Doctor,
    /// Print this computer's invite line (the reusable link, unless a flag says otherwise).
    Invite {
        /// Replace the reusable link; the old one stops working.
        #[arg(long, conflicts_with_all = ["once", "open"])]
        new: bool,
        /// A link for one join, valid 24 hours.
        #[arg(long, conflicts_with = "open")]
        once: bool,
        /// A link whose joins need no Allow (founder and admins).
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
    /// Remove a member (founder and admins); the invite link is replaced.
    Remove { name: String },
    /// Leave the team; this computer is on its own network again.
    Leave,
    /// Join the network an invite link is for (this computer must be on its own).
    Join {
        link: String,
        /// The name you go by in the network (default: your user name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Allow a join request (`J<n>`).
    Approve { request: String },
    /// Decline a join request (`J<n>`).
    Decline { request: String },
    #[command(external_subcommand)]
    External(Vec<OsString>),
    /// Run by an agent's hook config: `hook <claude|codex|cursor|grok> <session-start|user-prompt-submit|post-tool-use|stop|session-end>`.
    ///
    /// Always exits 0; errors go to stderr.
    /// Debug builds only: LIGHTBULB_CONNECT_HOOK_CALLER=<pid>:<birth_micros> names the calling agent (tests).
    Hook { agent: String, event: String },
    /// Run the stdio MCP server an agent talks to (find_sessions, send_message, ...).
    ///
    /// Debug builds only: LIGHTBULB_CONNECT_HOOK_CALLER=<pid>:<birth_micros> names the calling agent (tests).
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
        /// copy, ~/.lightbulb/connect/bin/lightbulb-connect, which setup installs from this one).
        #[arg(long)]
        executable: Option<std::path::PathBuf>,
        /// The MCP server entry's name, also used in Cursor's and Grok's allow rules.
        #[arg(long, default_value = setup::DEFAULT_SERVER)]
        server_name: String,
    },
}

/// Exits with `code` unless it is 0.
fn exit(code: i32) {
    if code != 0 {
        std::process::exit(code);
    }
}

fn main() -> lightbulb_connect::Result<()> {
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
        Command::Version => println!("lightbulb-connect {}", env!("CARGO_PKG_VERSION")),
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
        Command::Receipt { request_id, from } => exit(cli::receipt(&request_id, from)?),
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
        Command::Join { link, name } => exit(cli::join(&link, name)?),
        Command::Approve { request } => exit(cli::answer_join(&request, true)?),
        Command::Decline { request } => exit(cli::answer_join(&request, false)?),
        Command::External(args) => exit(cli::forward(args)?),
        Command::Mcp { server_name } => mcp::run(server_name)?,
        Command::Setup {
            remove,
            executable,
            server_name,
        } => {
            if !setup::valid_server_name(&server_name) {
                return Err("--server-name takes letters, digits, - and _".into());
            }
            if let Some(e) = executable.as_ref().filter(|e| !remove && !e.exists()) {
                return Err(format!("--executable {} does not exist", e.display()).into());
            }
            let home = cli::home()?;
            if !remove {
                cli::install_running(&home)?;
            }
            // Wired to the canonical copy, never to wherever this binary was built.
            let executable = executable.unwrap_or_else(|| cli::canonical(&home));
            for agent in setup::run(&home, &executable, &server_name, remove).agents {
                println!("{}", agent.line());
            }
            if remove {
                // legacy (Plan A install): its hook and MCP entries name the old binary.
                for agent in
                    setup::run(&home, &cli::legacy_canonical(&home), &server_name, true).agents
                {
                    if !agent.wrote.is_empty() {
                        println!("{}", agent.line());
                    }
                }
                cli::legacy_uninstall(&home)?;
            }
            if cfg!(target_os = "macos") {
                // `--remove` is the off switch: the job and the running daemon go too.
                exit(if remove {
                    cli::daemon_uninstall()?
                } else {
                    cli::install_job(&home, &executable)?
                });
            }
        }
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
                    let _ = writeln!(
                        std::io::stderr(),
                        "lightbulb-connect hook {agent} {event}: {e}"
                    );
                }
            }
        }
    }
    Ok(())
}
