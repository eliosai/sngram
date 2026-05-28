//! `sngram` — learn a sparse n-gram weight table by streaming HF datasets.
//!
//! Single sequential loop, one file at a time, never stops on rate limits.
//! Two commands: `learn` and `inspect`.

#![recursion_limit = "512"]

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};

use sngram_cli::{learn, paths};

const GB: u64 = 1_000_000_000;
const TB: u64 = 1_000 * GB;

const MILESTONES: &[(u64, &str)] = &[
    (GB, "1gb"), (10 * GB, "10gb"), (50 * GB, "50gb"), (100 * GB, "100gb"),
    (TB, "1tb"), (5 * TB, "5tb"), (10 * TB, "10tb"), (15 * TB, "15tb"),
    (25 * TB, "25tb"), (30 * TB, "30tb"), (40 * TB, "40tb"), (45 * TB, "45tb"),
];

#[derive(Parser)]
#[command(name = "sngram", version, about = "Sparse n-gram weight table learner")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Learn the weight table. Sequential, one file at a time, never stops on rate limits.
    Learn {
        #[arg(long)]
        mint_dir: Option<PathBuf>,
    },
    /// Inspect a minted weight table.
    Inspect {
        path: PathBuf,
        #[arg(long, default_value_t = 20)]
        top: usize,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Commands::Learn { mint_dir } => learn_cmd(mint_dir),
        Commands::Inspect { path, top } => inspect(&path, top),
    }
}

fn learn_cmd(mint_dir: Option<PathBuf>) -> anyhow::Result<()> {
    let token = require_hf_token()?;
    let mint_dir = mint_dir.unwrap_or_else(paths::default_mint_dir);
    paths::ensure_dir(&paths::data_dir())?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building runtime")?;
    rt.block_on(learn::learn(token, mint_dir, MILESTONES.to_vec()))
}

fn require_hf_token() -> anyhow::Result<String> {
    if let Ok(t) = std::env::var("HF_TOKEN") {
        return Ok(t);
    }
    let env = Path::new(".env");
    if env.exists() {
        let s = std::fs::read_to_string(env)?;
        for line in s.lines() {
            if let Some(v) = line.strip_prefix("HF_TOKEN=") {
                return Ok(v.trim().to_string());
            }
        }
    }
    anyhow::bail!("HF_TOKEN not set. Export it or add it to .env.")
}

fn inspect(path: &Path, top: usize) -> anyhow::Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let table = sngram_types::WeightTable::from_bytes(&bytes)
        .map_err(|e| anyhow::anyhow!("invalid weight table: {e}"))?;
    let mut pairs: Vec<(u32, u8, u8)> = Vec::with_capacity(256 * 256);
    for c1 in 0u8..=255 {
        for c2 in 0u8..=255 {
            pairs.push((table.weight(c1, c2), c1, c2));
        }
    }
    pairs.sort_unstable();
    println!("commonest bigrams (lowest weight):");
    for &(w, a, b) in pairs.iter().take(top) {
        println!("  {:<8} {}", w, show(a, b));
    }
    println!("rarest bigrams (highest weight):");
    for &(w, a, b) in pairs.iter().rev().take(top) {
        println!("  {:<8} {}", w, show(a, b));
    }
    Ok(())
}

fn show(a: u8, b: u8) -> String {
    format!("{}{}", a.escape_ascii(), b.escape_ascii())
}
