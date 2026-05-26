//! Learn subcommand.

use std::sync::Arc;

use crate::{counter, engine, session};

/// # Errors
///
/// Returns error on session conflict or streaming failure.
pub fn run(name: &str, _verbose: bool, quiet: bool) -> anyhow::Result<()> {
    session::create(name)?;
    write_lock(name)?;
    let result = engine::run(name, Arc::new(counter::BigramCounter::new()), 0, quiet);
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
