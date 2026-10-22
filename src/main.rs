use clap::{Parser, Subcommand};
use lightbulb_network::{cli, daemon, hook, mcp, paths, setup};
use std::ffi::OsString;
use std::io::{Read, Write};

#[derive(Parser)]
#[command(name = "lightbulb-network", version)]
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
    /// List the coding sessions on this computer.
    Who {
        /// Print the raw session list as JSON.
        #[arg(long)]
        json: bool,
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
    /// Leave the network.
    Leave,
    #[command(external_subcommand)]
    External(Vec<OsString>),
    /// Run by an agent's hook config: `hook <claude|codex|cursor|grok> <session-start|user-prompt-submit|post-tool-use|stop|session-end>`.
    ///
    /// Always exits 0; errors go to stderr.
    /// Debug builds only: LIGHTBULB_NETWORK_HOOK_CALLER=<pid>:<birth_micros> names the calling agent (tests).
    Hook { agent: String, event: String },
    /// Run the stdio MCP server an agent talks to (find_sessions, send_message, ...).
    ///
    /// Debug builds only: LIGHTBULB_NETWORK_HOOK_CALLER=<pid>:<birth_micros> names the calling agent (tests).
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
        /// The engine binary the hooks and MCP entries run (default: this one).
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

fn main() -> lightbulb_network::Result<()> {
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
        Command::Version => println!("lightbulb-network {}", env!("CARGO_PKG_VERSION")),
        Command::Daemon { install: true, .. } => exit(cli::daemon_install(
            &std::env::current_exe()?.canonicalize()?,
        )?),
        Command::Daemon {
            uninstall: true, ..
        } => exit(cli::daemon_uninstall()?),
        Command::Daemon { .. } => {
            tokio::runtime::Runtime::new()?.block_on(daemon::run(&paths::state_dir()))?
        }
        Command::Who { json } => exit(cli::who(json)?),
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
        Command::Leave => cli::leave(),
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
            let executable = match executable {
                Some(e) => e,
                None => std::env::current_exe()?.canonicalize()?,
            };
            let home = dirs::home_dir().ok_or("no home directory")?;
            for agent in setup::run(&home, &executable, &server_name, remove).agents {
                println!("{}", agent.line());
            }
            if !remove {
                // The alias line prints once; the job needs the canonical copy it makes.
                if cfg!(target_os = "macos") {
                    exit(cli::daemon_install(&executable)?);
                } else {
                    println!("{}", cli::install_alias(&home, &executable)?);
                }
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
                        "lightbulb-network hook {agent} {event}: {e}"
                    );
                }
            }
        }
    }
    Ok(())
}
