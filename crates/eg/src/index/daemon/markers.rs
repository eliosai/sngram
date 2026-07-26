//! Marker files the daemon writes under one watched tree's runtime directory.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

const RUNTIME_DIR_NAME: &str = "runtime";
pub const WATCHER_READY_FILE_NAME: &str = "watcher-ready";
pub const WATCH_REFUSED_FILE_NAME: &str = "watch-refused";
pub const JOURNAL_CLEAN_FILE_NAME: &str = "journal-clean";
pub const OWNER_FILE_NAME: &str = "daemon-owner";
pub const LEASE_FILE_NAME: &str = "lease";

/// Directory holding one tree's coordination markers
pub fn runtime_dir(state_root: &Path) -> PathBuf {
    state_root.join(RUNTIME_DIR_NAME)
}

/// Lease file a foreground query renews and holds open while it searches
pub fn lease_path(state_root: &Path) -> PathBuf {
    runtime_dir(state_root).join(LEASE_FILE_NAME)
}

/// Marker naming a tree the watcher covers whole
pub fn watcher_ready_path(state_root: &Path) -> PathBuf {
    runtime_dir(state_root).join(WATCHER_READY_FILE_NAME)
}

/// Say the watcher covers every directory this tree's index was built from
pub fn mark_watcher_ready(state_root: &Path) -> std::io::Result<()> {
    restate(state_root, WATCHER_READY_FILE_NAME, &process_id())
}

/// Record why this tree cannot be watched so queries fail fast
pub fn mark_watch_refused(state_root: &Path, reason: &str) -> std::io::Result<()> {
    write_marker(state_root, WATCH_REFUSED_FILE_NAME, reason)
}

pub fn clear_watch_refusal(state_root: &Path) {
    clear(state_root, WATCH_REFUSED_FILE_NAME);
}

pub fn clear_watcher_ready(state_root: &Path) {
    let _ = fs::remove_file(watcher_ready_path(state_root));
}

pub fn mark_owner(state_root: &Path, owner: &str) -> std::io::Result<()> {
    restate(state_root, OWNER_FILE_NAME, owner)
}

pub fn mark_lease_live(state_root: &Path) -> std::io::Result<()> {
    write_marker(state_root, LEASE_FILE_NAME, &process_id())
}

/// Freshness proof for a generation the daemon knows nothing has outrun
pub fn mark_journal_clean(state_root: &Path) -> std::io::Result<()> {
    write_marker(state_root, JOURNAL_CLEAN_FILE_NAME, &process_id())
}

pub fn clear_journal_clean(state_root: &Path) {
    clear(state_root, JOURNAL_CLEAN_FILE_NAME);
}

pub fn clear(state_root: &Path, name: &str) {
    let _ = fs::remove_file(runtime_dir(state_root).join(name));
}

fn process_id() -> String {
    std::process::id().to_string()
}

/// Write a marker read for its body, leaving an already correct one alone
fn restate(state_root: &Path, name: &str, body: &str) -> std::io::Result<()> {
    if fs::read_to_string(runtime_dir(state_root).join(name))
        .is_ok_and(|held| held.trim_end() == body)
    {
        return Ok(());
    }
    write_marker(state_root, name, body)
}

fn write_marker(state_root: &Path, name: &str, body: &str) -> std::io::Result<()> {
    let runtime = runtime_dir(state_root);
    fs::create_dir_all(&runtime)?;
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(runtime.join(name))?;
    writeln!(file, "{body}")?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::{
        JOURNAL_CLEAN_FILE_NAME, WATCH_REFUSED_FILE_NAME, clear_journal_clean, clear_watch_refusal,
        mark_journal_clean, mark_watch_refused, runtime_dir,
    };

    fn scratch(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("eg-markers-{name}-"))
            .tempdir()
            .expect("scratch dir")
    }

    #[test]
    fn a_marker_is_written_and_cleared_under_the_runtime_directory() {
        let guard = scratch("clean");
        let state_root = guard.path();
        let path = runtime_dir(state_root).join(JOURNAL_CLEAN_FILE_NAME);

        mark_journal_clean(state_root).expect("mark");
        assert!(path.exists());

        clear_journal_clean(state_root);
        assert!(!path.exists());
    }

    #[test]
    fn a_refusal_keeps_the_reason_it_was_written_with() {
        let guard = scratch("refused");
        let state_root = guard.path();

        mark_watch_refused(state_root, "out of watches").expect("mark");

        let text = std::fs::read_to_string(runtime_dir(state_root).join(WATCH_REFUSED_FILE_NAME))
            .expect("read");
        assert_eq!("out of watches", text.trim());

        clear_watch_refusal(state_root);
        assert!(
            !runtime_dir(state_root)
                .join(WATCH_REFUSED_FILE_NAME)
                .exists()
        );
    }
}
