//! `sngram` — Sparse n-gram weight table learner.

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

mod checkpoint;
mod cmd;
mod counter;
mod datasets;
mod engine;
mod progress;
mod resources;
mod session;

use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "sngram", version, about = "Sparse n-gram weight table learner")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Verbose: stream worker logs below progress bars.
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Quiet: progress bars only, no header.
    #[arg(short, long, global = true)]
    quiet: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Learn a weight table from the canonical datasets.
    Learn {
        /// Session name for checkpoint storage.
        #[arg(short, long)]
        name: String,
    },

    /// Resume an interrupted learning session.
    Resume {
        /// Session name to resume.
        #[arg(short, long)]
        name: String,
    },

    /// Inspect a completed weight table.
    Inspect {
        /// Session name to inspect.
        #[arg(short, long)]
        name: String,

        /// Number of rarest bigrams to show.
        #[arg(long, default_value = "20")]
        top_rare: usize,

        /// Number of most common bigrams to show.
        #[arg(long, default_value = "20")]
        top_common: usize,
    },

    /// Export a completed table to a file.
    Export {
        /// Session name to export.
        #[arg(short, long)]
        name: String,

        /// Output format.
        #[arg(short, long, value_enum)]
        format: ExportFormat,

        /// Output file path.
        #[arg(short, long)]
        output: PathBuf,
    },

    /// Show session status.
    Status {
        /// Session name (omit to list all sessions).
        #[arg(short, long)]
        name: Option<String>,
    },

    /// Delete a paused or completed session.
    Delete {
        /// Session name to delete.
        #[arg(short, long)]
        name: String,
    },
}

/// Output formats for weight table export.
#[derive(Clone, ValueEnum)]
enum ExportFormat {
    /// Binary format (256 KB, embeddable via `include_bytes!`).
    Bin,
    /// JSON with metadata and top bigrams.
    Json,
    /// CSV: c1, c2, weight.
    Csv,
    /// Rust source: `const TABLE: [u32; 65536] = [...]`.
    Rust,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Learn { name } => cmd::learn::run(&name, cli.verbose, cli.quiet),
        Commands::Resume { name } => cmd::resume::run(&name, cli.verbose, cli.quiet),
        Commands::Inspect {
            name,
            top_rare,
            top_common,
        } => cmd::inspect::run(&name, top_rare, top_common),
        Commands::Export {
            name,
            format,
            output,
        } => cmd::export::run(&name, format, &output),
        Commands::Status { name } => cmd::status::run(name.as_deref()),
        Commands::Delete { name } => cmd::delete::run(&name),
    }
}
