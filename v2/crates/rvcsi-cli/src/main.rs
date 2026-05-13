//! `rvcsi` — the rvCSI command-line tool (ADR-095 FR7).
//!
//! Subcommands: `inspect`, `replay`, `stream`, `events`, `health`, `calibrate`,
//! `export`. Long-running capture / WebSocket streaming live in the (not-yet-
//! shipped) `rvcsi-daemon`; this CLI works against `.rvcsi` capture files and
//! Nexmon record dumps.

mod commands;

use std::io::{self, Write};

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rvcsi", version, about = "rvCSI — edge RF sensing runtime CLI", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Transcode a Nexmon record dump into a `.rvcsi` capture (validating each frame).
    Record {
        /// Path to a buffer of "rvCSI Nexmon records" (the napi-c shim format).
        #[arg(long = "in")]
        input: String,
        /// Path to write the `.rvcsi` capture file.
        #[arg(long = "out")]
        output: String,
        /// Source id to stamp on the capture.
        #[arg(long, default_value = "nexmon")]
        source_id: String,
        /// Session id for the capture.
        #[arg(long, default_value_t = 0)]
        session: u64,
    },
    /// Summarize a `.rvcsi` capture file (frame count, channels, quality, ...).
    Inspect {
        /// Path to a `.rvcsi` capture file.
        path: String,
        /// Emit machine-readable JSON instead of a human summary.
        #[arg(long)]
        json: bool,
    },
    /// Replay a `.rvcsi` capture, emitting one line per frame.
    Replay {
        /// Path to a `.rvcsi` capture file.
        path: String,
        /// Emit each frame as a full JSON object instead of a compact line.
        #[arg(long)]
        json: bool,
        /// Stop after this many frames.
        #[arg(long)]
        limit: Option<usize>,
        /// Real-time pacing multiplier. Accepted for compatibility but not
        /// enforced by the CLI (the `rvcsi-daemon` paces real-time replay);
        /// a value other than `1.0` is noted on stderr.
        #[arg(long, default_value_t = 1.0)]
        speed: f32,
    },
    /// Stream frames from a source to stdout as JSON lines (a v0 stand-in for
    /// the daemon's WebSocket output). Currently supports `.rvcsi` files via `--in`.
    Stream {
        /// Path to a `.rvcsi` capture file to stream.
        #[arg(long = "in")]
        input: String,
        /// Output format (only `json` is supported in this build).
        #[arg(long, default_value = "json")]
        format: String,
        /// WebSocket port. Accepted but not served by the CLI — needs `rvcsi-daemon`.
        #[arg(long)]
        port: Option<u16>,
    },
    /// Replay a capture through the DSP + event pipeline and print the events.
    Events {
        /// Path to a `.rvcsi` capture file.
        path: String,
        /// Emit events as JSON instead of compact lines.
        #[arg(long)]
        json: bool,
    },
    /// Open a source, drain it, and print its `SourceHealth` as JSON.
    Health {
        /// Source slug: `file`, `replay`, `nexmon` (offline); `esp32`/`intel`/`atheros` need the daemon.
        #[arg(long)]
        source: String,
        /// Path / interface for the source (required for `file`/`replay`/`nexmon`).
        #[arg(long)]
        target: Option<String>,
    },
    /// Learn a v0 baseline (per-subcarrier mean amplitude) from a capture.
    Calibrate {
        /// Path to a `.rvcsi` capture file.
        #[arg(long = "in")]
        input: String,
        /// Write the baseline JSON here instead of stdout.
        #[arg(long = "out")]
        output: Option<String>,
    },
    /// Export data derived from a capture.
    Export {
        #[command(subcommand)]
        target: ExportTarget,
    },
}

#[derive(Subcommand)]
enum ExportTarget {
    /// Window a capture and store each window's embedding into a JSONL RF-memory file.
    Ruvector(ExportRuvector),
}

#[derive(Args)]
struct ExportRuvector {
    /// Path to a `.rvcsi` capture file.
    #[arg(long = "in")]
    input: String,
    /// Path to the output JSONL RF-memory file.
    #[arg(long = "out")]
    output: String,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match cli.command {
        Command::Record { input, output, source_id, session } => {
            commands::record_from_nexmon(&mut out, &input, &output, &source_id, session)?
        }
        Command::Inspect { path, json } => commands::inspect(&mut out, &path, json)?,
        Command::Replay { path, json, limit, speed } => {
            if (speed - 1.0).abs() > f32::EPSILON {
                eprintln!("note: --speed {speed} is not enforced by the CLI; replaying as fast as possible");
            }
            commands::replay(&mut out, &path, json, limit)?;
        }
        Command::Stream { input, format, port } => {
            if format != "json" {
                anyhow::bail!("unsupported --format `{format}` (only `json` is available in this build)");
            }
            if let Some(p) = port {
                eprintln!("note: --port {p} (WebSocket) needs the rvcsi-daemon; streaming JSON lines to stdout instead");
            }
            commands::replay(&mut out, &input, true, None)?;
        }
        Command::Events { path, json } => commands::events(&mut out, &path, json)?,
        Command::Health { source, target } => commands::health(&mut out, &source, target.as_deref())?,
        Command::Calibrate { input, output } => commands::calibrate(&mut out, &input, output.as_deref())?,
        Command::Export { target } => match target {
            ExportTarget::Ruvector(a) => commands::export_ruvector(&mut out, &a.input, &a.output)?,
        },
    }
    out.flush()?;
    Ok(())
}
