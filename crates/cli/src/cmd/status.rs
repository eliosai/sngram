//! Show session status.

use crate::session;

/// # Errors
///
/// Returns error on filesystem failure.
pub fn run(name: Option<&str>) -> anyhow::Result<()> {
    match name {
        Some(n) => show_one(n),
        None => show_all(),
    }
}

fn show_one(name: &str) -> anyhow::Result<()> {
    let st = session::state(name);
    println!("  {name:<20} {}", format_state(st));
    Ok(())
}

fn show_all() -> anyhow::Result<()> {
    let sessions = session::list()?;
    if sessions.is_empty() {
        println!("  No sessions found.");
        return Ok(());
    }
    for (name, st) in &sessions {
        println!("  {name:<20} {}", format_state(*st));
    }
    Ok(())
}

fn format_state(st: session::State) -> &'static str {
    match st {
        session::State::New => "new",
        session::State::Paused => "paused",
        session::State::Completed => "completed",
        session::State::Running => "running",
    }
}
