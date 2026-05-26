//! Shared orchestration for learn and resume.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;

use crate::{checkpoint, counter, datasets, progress, resources, session};

/// Run the streaming engine, starting from `skip_datasets` already completed.
///
/// # Errors
///
/// Returns error on streaming or checkpoint failure.
pub fn run(
    name: &str,
    counter: Arc<counter::BigramCounter>,
    skip_datasets: usize,
    quiet: bool,
) -> anyhow::Result<()> {
    let profile = resources::MachineProfile::detect();
    let alloc = resources::ThreadAllocation::from_cores(profile.cores);
    let token = require_hf_token()?;

    if !quiet { print_header(&profile, &alloc); }

    let shutdown = Arc::new(AtomicBool::new(false));
    setup_signals(&shutdown)?;

    let workers = alloc.stack + alloc.fineweb + alloc.redpajama;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .context("building runtime")?;

    let completed = rt.block_on(stream_datasets(
        name, &counter, Some(token.as_str()), &shutdown, skip_datasets, quiet,
    ))?;

    if shutdown.load(Ordering::Relaxed) {
        checkpoint::save(&session::checkpoint(name), &counter, completed)?;
        if !quiet {
            println!("\n  Checkpoint saved. Resume: sngram resume --name {name}");
        }
        return Ok(());
    }

    std::fs::write(session::weights(name), counter.to_table_bytes())
        .context("writing weights")?;
    if !quiet { print_complete(name, &counter); }
    Ok(())
}

async fn stream_datasets(
    name: &str,
    counter: &Arc<counter::BigramCounter>,
    token: Option<&str>,
    shutdown: &Arc<AtomicBool>,
    skip: usize,
    quiet: bool,
) -> anyhow::Result<usize> {
    let remaining: Vec<_> = datasets::DATASETS.iter().enumerate()
        .skip(skip)
        .collect();

    let file_counts = list_files_for(&remaining, token).await?;
    let names: Vec<&str> = remaining.iter().map(|(_, ds)| ds.name).collect();
    let counts: Vec<u64> = file_counts.iter().map(|f| f.len() as u64).collect();
    let prog = if quiet { None } else { Some(progress::Progress::named(&names, &counts)) };

    let mut completed = skip;
    for (i, (ds_idx, ds)) in remaining.iter().enumerate() {
        let op = datasets::operator(ds, token)?;
        for path in &file_counts[i] {
            if shutdown.load(Ordering::Relaxed) { return Ok(completed); }
            if let Err(e) = datasets::stream_file(&op, path, ds.field, counter).await {
                eprintln!("  warning: {path}: {e:#}");
            }
            if let Some(p) = &prog { p.inc_bytes(i, 1); }
        }
        completed = ds_idx + 1;
        if let Some(p) = &prog { p.finish_dataset(i); }
        checkpoint::save(&session::checkpoint(name), counter, completed)?;
    }

    if let Some(p) = &prog { p.finish_all(); }
    Ok(completed)
}

async fn list_files_for(
    datasets: &[(usize, &datasets::Dataset)],
    token: Option<&str>,
) -> anyhow::Result<Vec<Vec<String>>> {
    let mut all = Vec::with_capacity(datasets.len());
    for (_, ds) in datasets {
        let op = datasets::operator(ds, token)?;
        all.push(datasets::list_files(&op, ds.prefix).await?);
    }
    Ok(all)
}

fn require_hf_token() -> anyhow::Result<String> {
    if let Ok(token) = std::env::var("HF_TOKEN") {
        return Ok(token);
    }
    let env_path = std::path::Path::new(".env");
    if env_path.exists() {
        let content = std::fs::read_to_string(env_path)?;
        for line in content.lines() {
            if let Some(val) = line.strip_prefix("HF_TOKEN=") {
                return Ok(val.trim().to_owned());
            }
        }
    }
    anyhow::bail!("HF_TOKEN not set. Export it or add to .env file.")
}

fn setup_signals(shutdown: &Arc<AtomicBool>) -> anyhow::Result<()> {
    let flag = shutdown.clone();
    signal_hook::flag::register(signal_hook::consts::SIGINT, flag.clone())
        .context("registering SIGINT")?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, flag.clone())
        .context("registering SIGTERM")?;
    signal_hook::flag::register_conditional_default(
        signal_hook::consts::SIGINT, flag,
    ).context("registering double-SIGINT")?;
    Ok(())
}

fn print_header(
    profile: &resources::MachineProfile,
    alloc: &resources::ThreadAllocation,
) {
    println!();
    println!("  sngram - Sparse N-gram Weight Table Learner");
    println!();
    println!("  Machine");
    println!("    CPU     {} cores ({})", profile.cores, profile.arch);
    println!("    RAM     {} GB", profile.ram_mb / 1024);
    println!("    OS      {}", profile.os);
    println!();
    println!("  Allocation");
    println!("    the-stack-v2       {:>2} threads", alloc.stack);
    println!("    fineweb-2          {:>2} threads", alloc.fineweb);
    println!("    redpajama          {:>2} threads", alloc.redpajama);
    println!("    system             {:>2} threads", alloc.reserved);
    println!();
}

fn print_complete(name: &str, counter: &counter::BigramCounter) {
    println!();
    println!("  Session '{name}' complete.");
    println!("    Pairs   {}", counter.pairs_processed());
    println!("    Files   {}", counter.files_processed());
    println!("    Output  {}", session::weights(name).display());
    println!();
}
