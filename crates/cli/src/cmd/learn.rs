//! Learn subcommand — orchestrates dataset streaming.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;

use crate::{checkpoint, counter, datasets, progress, resources, session};

/// # Errors
///
/// Returns error on session conflict or streaming failure.
pub fn run(name: &str, verbose: bool, quiet: bool) -> anyhow::Result<()> {
    session::create(name)?;
    let counter = Arc::new(counter::BigramCounter::new());
    let token = std::env::var("HF_TOKEN").ok();
    run_learning(name, &counter, token.as_deref(), verbose, quiet)
}

fn run_learning(
    name: &str,
    counter: &Arc<counter::BigramCounter>,
    token: Option<&str>,
    verbose: bool,
    quiet: bool,
) -> anyhow::Result<()> {
    let profile = resources::MachineProfile::detect();
    let alloc = resources::ThreadAllocation::from_cores(profile.cores);

    if !quiet { print_header(&profile, &alloc); }

    let shutdown = Arc::new(AtomicBool::new(false));
    setup_signal_handler(&shutdown);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(alloc.stack + alloc.fineweb + alloc.redpajama)
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    rt.block_on(stream_all(name, counter, token, &shutdown, quiet))?;

    if shutdown.load(Ordering::Relaxed) {
        checkpoint::save(&session::checkpoint(name), counter)?;
        if !quiet { println!("\n  Checkpoint saved. Resume with: sngram resume --name {name}"); }
        return Ok(());
    }

    let table_bytes = counter.to_table_bytes();
    std::fs::write(session::weights(name), &table_bytes).context("writing weights")?;
    if !quiet { print_complete(name, counter); }
    Ok(())
}

async fn stream_all(
    name: &str,
    counter: &Arc<counter::BigramCounter>,
    token: Option<&str>,
    shutdown: &Arc<AtomicBool>,
    quiet: bool,
) -> anyhow::Result<()> {
    let file_counts = list_all_files(token).await?;
    let byte_counts: Vec<u64> = file_counts.iter().map(|f| f.len() as u64).collect();
    let prog = if quiet { None } else { Some(progress::Progress::new(&byte_counts)) };

    for (ds_idx, ds) in datasets::DATASETS.iter().enumerate() {
        let files = &file_counts[ds_idx];
        for path in files {
            if shutdown.load(Ordering::Relaxed) { return Ok(()); }
            let bytes = datasets::stream_file(ds, path, token, counter).await?;
            if let Some(p) = &prog { p.inc_bytes(ds_idx, bytes); }
        }
        if let Some(p) = &prog { p.finish_dataset(ds_idx); }

        checkpoint::save(&session::checkpoint(name), counter)?;
    }

    if let Some(p) = &prog { p.finish_all(); }
    Ok(())
}

async fn list_all_files(token: Option<&str>) -> anyhow::Result<Vec<Vec<String>>> {
    let mut all = Vec::with_capacity(datasets::DATASETS.len());
    for ds in datasets::DATASETS {
        let files = datasets::list_files(ds, token).await?;
        all.push(files);
    }
    Ok(all)
}

fn setup_signal_handler(shutdown: &Arc<AtomicBool>) {
    let flag = shutdown.clone();
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, flag.clone());
    let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, flag);
}

fn print_header(profile: &resources::MachineProfile, alloc: &resources::ThreadAllocation) {
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
    println!("    Pairs processed  {}", counter.pairs_processed());
    println!("    Files processed  {}", counter.files_processed());
    println!("    Output           {}", session::weights(name).display());
    println!();
}
