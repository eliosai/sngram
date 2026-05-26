//! Resume subcommand.

use std::sync::Arc;

use crate::{checkpoint, engine, session};

/// # Errors
///
/// Returns error if session cannot be resumed.
pub fn run(name: &str, _verbose: bool, quiet: bool) -> anyhow::Result<()> {
    session::resume(name)?;
    write_lock(name)?;
    let data = checkpoint::restore(&session::checkpoint(name))?;

    if !quiet {
        println!();
        println!("  Resuming '{name}'");
        println!("    Datasets done  {}", data.completed_datasets);
        println!("    Pairs so far   {}", data.counter.pairs_processed());
        println!("    Files so far   {}", data.counter.files_processed());
        println!();
    }

    let result = engine::run(name, Arc::new(data.counter), data.completed_datasets, quiet);
    remove_lock(name);
    result
}

fn write_lock(name: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    let pid = std::process::id().to_string();
    std::fs::write(session::lock(name), pid.as_bytes())
        .context("writing lock")
}

fn remove_lock(name: &str) {
    let _ = std::fs::remove_file(session::lock(name));
}
