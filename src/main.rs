use clap::{Parser, Subcommand};
use lightbulb_network::{daemon, hook, paths};
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
