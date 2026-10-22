use clap::{Parser, Subcommand};
use lightbulb_network::{daemon, hook, mcp, paths, setup};
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
    /// Run the per-computer daemon in the foreground.
    Daemon,
    /// Run by an agent's hook config: `hook <claude|codex|cursor|grok> <session-start|user-prompt-submit|post-tool-use|stop|session-end>`.
    ///
    /// Always exits 0; errors go to stderr.
    /// Debug builds only: LIGHTBULB_NETWORK_HOOK_CALLER=<pid>:<birth_micros> names the calling agent (tests).
    Hook { agent: String, event: String },
    /// Run the stdio MCP server an agent talks to (find_sessions, send_message, ...).
    ///
    /// Debug builds only: LIGHTBULB_NETWORK_HOOK_CALLER=<pid>:<birth_micros> names the calling agent (tests).
    Mcp,
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
        #[arg(long, default_value = "lightbulb")]
        server_name: String,
    },
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
        Command::Daemon => {
            tokio::runtime::Runtime::new()?.block_on(daemon::run(&paths::state_dir()))?
        }
        Command::Mcp => mcp::run()?,
        Command::Setup {
            remove,
            executable,
            server_name,
        } => {
            if server_name.is_empty()
                || !server_name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
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
