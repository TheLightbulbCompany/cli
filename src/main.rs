use clap::{Parser, Subcommand};

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
}

fn main() {
    match Cli::parse().command {
        Command::Version => println!("lightbulb-network {}", env!("CARGO_PKG_VERSION")),
    }
}
