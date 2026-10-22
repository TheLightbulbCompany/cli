use clap::{Parser, Subcommand};
use lightbulb_network::{daemon, paths};

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
}

fn main() -> lightbulb_network::Result<()> {
    match Cli::parse().command {
        Command::Version => println!("lightbulb-network {}", env!("CARGO_PKG_VERSION")),
        Command::Daemon => {
            tokio::runtime::Runtime::new()?.block_on(daemon::run(&paths::state_dir()))?
        }
    }
    Ok(())
}
