use clap::{Parser, Subcommand};
use lightbulb_network::{daemon, hook, paths};
use std::io::Read;

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
    /// Always exits 0; errors go to stderr.
    Hook { agent: String, event: String },
}

fn main() -> lightbulb_network::Result<()> {
    match Cli::parse().command {
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
                Ok(Some(out)) => println!("{out}"),
                Ok(None) => {}
                Err(e) => eprintln!("lightbulb-network hook {agent} {event}: {e}"),
            }
        }
    }
    Ok(())
}
