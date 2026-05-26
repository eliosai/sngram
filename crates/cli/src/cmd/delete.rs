//! Delete a session.

use crate::session;

/// # Errors
///
/// Returns error if session not found or is running.
pub fn run(name: &str) -> anyhow::Result<()> {
    session::delete(name)?;
    println!("  Deleted session '{name}'");
    Ok(())
}
