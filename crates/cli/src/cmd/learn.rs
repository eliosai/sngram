//! Learn subcommand.

use std::sync::Arc;

use crate::{counter, engine, session};

/// # Errors
///
/// Returns error on session conflict or streaming failure.
pub fn run(name: &str, _verbose: bool, quiet: bool) -> anyhow::Result<()> {
    session::create(name)?;
    let _lock = session::LockGuard::acquire(name)?;
    engine::run(name, Arc::new(counter::BigramCounter::new()), 0, quiet)
}
