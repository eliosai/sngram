//! Session lifecycle via filesystem-implicit state.
//!
//! State is derived from which files exist in the session directory:
//! - `lock`            → currently running
//! - `checkpoint.redb` → paused (can resume)
//! - `weights.bin`     → completed

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, bail};

/// Filesystem-derived session state.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum State {
    New,
    Paused,
    Completed,
    Running,
}

fn base_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "sngram")
        .map(|p| p.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".sngram"))
        .join("sessions")
}

/// Session directory path.
#[must_use]
pub fn dir(name: &str) -> PathBuf { base_dir().join(name) }

/// Checkpoint database path.
#[must_use]
pub fn checkpoint(name: &str) -> PathBuf { dir(name).join("checkpoint.redb") }

/// Final weight table path.
#[must_use]
pub fn weights(name: &str) -> PathBuf { dir(name).join("weights.bin") }

/// Lock file path.
#[must_use]
pub fn lock(name: &str) -> PathBuf { dir(name).join("lock") }

/// RAII lock guard — removes lock file on drop (panic-safe).
pub struct LockGuard {
    name: String,
}

impl LockGuard {
    /// # Errors
    ///
    /// Returns error if lock file cannot be written.
    pub fn acquire(name: &str) -> anyhow::Result<Self> {
        let pid = std::process::id().to_string();
        std::fs::write(lock(name), pid.as_bytes())
            .context("writing lock")?;
        Ok(Self { name: name.to_owned() })
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(lock(&self.name));
    }
}

/// Derive session state from filesystem.
#[must_use]
pub fn state(name: &str) -> State {
    let d = dir(name);
    if !d.exists() { return State::New; }
    if lock(name).exists() { return State::Running; }
    if weights(name).exists() { return State::Completed; }
    if checkpoint(name).exists() { return State::Paused; }
    State::New
}

/// # Errors
///
/// Returns error if session already exists.
pub fn create(name: &str) -> anyhow::Result<PathBuf> {
    match state(name) {
        State::New => {},
        State::Paused => bail!("session '{name}' exists, use resume"),
        State::Completed => bail!("session '{name}' already completed"),
        State::Running => bail!("session '{name}' is currently running"),
    }
    let d = dir(name);
    fs::create_dir_all(&d).context("creating session directory")?;
    Ok(d)
}

/// # Errors
///
/// Returns error if session cannot be resumed.
pub fn resume(name: &str) -> anyhow::Result<PathBuf> {
    match state(name) {
        State::Paused => Ok(dir(name)),
        State::New => bail!("session '{name}' not found"),
        State::Completed => bail!("session '{name}' already completed"),
        State::Running => bail!("session '{name}' is currently running"),
    }
}

/// # Errors
///
/// Returns error if session not found or is running.
pub fn delete(name: &str) -> anyhow::Result<()> {
    match state(name) {
        State::Running => bail!("session '{name}' is currently running"),
        State::New => bail!("session '{name}' not found"),
        _ => {},
    }
    fs::remove_dir_all(dir(name)).context("deleting session")
}

/// List all sessions with their states.
///
/// # Errors
///
/// Returns error on filesystem failure.
pub fn list() -> anyhow::Result<Vec<(String, State)>> {
    let base = base_dir();
    if !base.exists() { return Ok(Vec::new()); }
    let mut result = Vec::new();
    for entry in fs::read_dir(&base)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() { continue; }
        let Ok(name) = entry.file_name().into_string() else { continue; };
        let st = state(&name);
        result.push((name, st));
    }
    result.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        old: Option<String>,
    }

    impl EnvGuard {
        fn set(path: &std::path::Path) -> Self {
            let old = std::env::var("XDG_DATA_HOME").ok();
            // SAFETY: tests run with --test-threads=1, no concurrent env access
            #[allow(unsafe_code, reason = "test env isolation")]
            unsafe { std::env::set_var("XDG_DATA_HOME", path); }
            Self { old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            #[allow(unsafe_code, reason = "restore env on drop")]
            match &self.old {
                Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v); },
                None => unsafe { std::env::remove_var("XDG_DATA_HOME"); },
            }
        }
    }

    fn isolated(f: impl FnOnce()) {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set(tmp.path());
        f();
    }

    #[test]
    fn new_session_is_new() {
        isolated(|| assert_eq!(state("x"), State::New));
    }

    #[test]
    fn create_makes_directory() {
        isolated(|| assert!(create("t").unwrap().exists()));
    }

    #[test]
    fn create_duplicate_errors() {
        isolated(|| {
            create("d").unwrap();
            fs::write(checkpoint("d"), b"").unwrap();
            assert!(create("d").is_err());
        });
    }

    #[test]
    fn paused_detected() {
        isolated(|| {
            create("s").unwrap();
            fs::write(checkpoint("s"), b"").unwrap();
            assert_eq!(state("s"), State::Paused);
        });
    }

    #[test]
    fn completed_detected() {
        isolated(|| {
            create("s").unwrap();
            fs::write(weights("s"), b"").unwrap();
            assert_eq!(state("s"), State::Completed);
        });
    }

    #[test]
    fn running_detected() {
        isolated(|| {
            create("s").unwrap();
            fs::write(lock("s"), b"").unwrap();
            assert_eq!(state("s"), State::Running);
        });
    }

    #[test]
    fn resume_paused_works() {
        isolated(|| {
            create("s").unwrap();
            fs::write(checkpoint("s"), b"").unwrap();
            assert!(resume("s").is_ok());
        });
    }

    #[test]
    fn resume_completed_errors() {
        isolated(|| {
            create("s").unwrap();
            fs::write(weights("s"), b"").unwrap();
            assert!(resume("s").is_err());
        });
    }

    #[test]
    fn delete_removes() {
        isolated(|| {
            create("s").unwrap();
            fs::write(checkpoint("s"), b"").unwrap();
            delete("s").unwrap();
            assert_eq!(state("s"), State::New);
        });
    }

    #[test]
    fn delete_running_errors() {
        isolated(|| {
            create("s").unwrap();
            fs::write(lock("s"), b"").unwrap();
            assert!(delete("s").is_err());
        });
    }

    #[test]
    fn list_returns_sorted() {
        isolated(|| {
            create("b").unwrap();
            create("a").unwrap();
            fs::write(weights("b"), b"").unwrap();
            let sessions = list().unwrap();
            assert_eq!(sessions.len(), 2);
            assert_eq!(sessions[0].0, "a");
            assert_eq!(sessions[1].1, State::Completed);
        });
    }
}
