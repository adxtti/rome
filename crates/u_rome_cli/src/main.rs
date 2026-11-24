//! The `rome` command line tool.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rome", version, about = "Unofficial ROME command line tool")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the asset kind detected from a file's leading bytes.
    Detect { path: std::path::PathBuf },
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    match Cli::parse().command {
        Command::Detect { path } => {
            let data = std::fs::read(&path)?;
            println!("{}", u_rome_core::detect(&data).label());
        }
    }
    Ok(())
}
