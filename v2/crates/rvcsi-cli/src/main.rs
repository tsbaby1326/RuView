//! `rvcsi` — the rvCSI command-line tool (skeleton; completed during integration).
//!
//! Subcommands (ADR-095 FR7): `inspect`, `replay`, `stream`, `calibrate`,
//! `health`, `export`. The skeleton wires `inspect` so the binary is usable;
//! the rest are filled in by the CLI swarm agent / integration step.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rvcsi", version, about = "rvCSI — edge RF sensing runtime CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print a summary of a capture file (frame count, channels, quality).
    Inspect {
        /// Path to a `.rvcsi` capture file.
        path: String,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Inspect { path } => {
            println!("rvcsi inspect: {path} (not yet implemented in the skeleton)");
            Ok(())
        }
    }
}
