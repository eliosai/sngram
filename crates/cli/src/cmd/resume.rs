//! Resume an interrupted learning session.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;

use crate::{checkpoint, counter, datasets, progress, resources, session};

/// # Errors
///
/// Returns error if session cannot be resumed.
pub fn run(name: &str, verbose: bool, quiet: bool) -> anyhow::Result<()> {
    session::resume(name)?;
    let cp_path = session::checkpoint(name);
    let counter = Arc::new(checkpoint::restore(&cp_path)?);
    let token = std::env::var("HF_TOKEN").ok();

    if !quiet {
        println!();
        println!("  Resuming session '{name}'");
        println!("    Pairs so far    {}", counter.pairs_processed());
        println!("    Files so far    {}", counter.files_processed());
        println!();
    }

    run_remaining(name, &counter, token.as_deref(), verbose, quiet)
}

fn run_remaining(
    name: &str,
    counter: &Arc<counter::BigramCounter>,
    token: Option<&str>,
    _verbose: bool,
    quiet: bool,
) -> anyhow::Result<()> {
    let profile = resources::MachineProfile::detect();
    let alloc = resources::ThreadAllocation::from_cores(profile.cores);

    let shutdown = Arc::new(AtomicBool::new(false));
    let flag = shutdown.clone();
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, flag.clone());
    let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, flag);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(alloc.stack + alloc.fineweb + alloc.redpajama)
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    rt.block_on(stream_remaining(name, counter, token, &shutdown, quiet))?;

    if shutdown.load(Ordering::Relaxed) {
        checkpoint::save(&session::checkpoint(name), counter)?;
        if !quiet { println!("\n  Checkpoint saved. Resume with: sngram resume --name {name}"); }
        return Ok(());
    }

    let table_bytes = counter.to_table_bytes();
    std::fs::write(session::weights(name), &table_bytes).context("writing weights")?;
    if !quiet {
        println!();
        println!("  Session '{name}' complete.");
        println!("    Output  {}", session::weights(name).display());
    }
    Ok(())
}

async fn stream_remaining(
    name: &str,
    counter: &Arc<counter::BigramCounter>,
    token: Option<&str>,
    shutdown: &Arc<AtomicBool>,
    quiet: bool,
) -> anyhow::Result<()> {
    for (ds_idx, ds) in datasets::DATASETS.iter().enumerate() {
        let files = datasets::list_files(ds, token).await?;
        for path in &files {
            if shutdown.load(Ordering::Relaxed) { return Ok(()); }
            let _bytes = datasets::stream_file(ds, path, token, counter).await?;
        }
        checkpoint::save(&session::checkpoint(name), counter)?;
    }
    Ok(())
}
