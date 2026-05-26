//! Resume subcommand.

use std::sync::Arc;

use crate::{checkpoint, engine, session};

/// # Errors
///
/// Returns error if session cannot be resumed.
pub fn run(name: &str, _verbose: bool, quiet: bool) -> anyhow::Result<()> {
    session::resume(name)?;
    let _lock = session::LockGuard::acquire(name)?;
    let data = checkpoint::restore(&session::checkpoint(name))?;

    if !quiet {
        println!();
        println!("  Resuming '{name}'");
        println!("    Datasets done  {}", data.completed_datasets);
        println!("    Pairs so far   {}", data.counter.pairs_processed());
        println!("    Files so far   {}", data.counter.files_processed());
        println!();
    }

    engine::run(name, Arc::new(data.counter), data.completed_datasets, quiet)
}
