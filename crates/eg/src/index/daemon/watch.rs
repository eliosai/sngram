//! Filesystem invalidation for daemon-maintained index generations.

#[cfg(target_os = "linux")]
mod dirs;
#[cfg(target_os = "linux")]
mod events;
#[cfg(target_os = "linux")]
mod inotify;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod registered;
#[cfg(target_os = "linux")]
mod registry;

#[cfg(target_os = "linux")]
pub use linux::Watcher;

/// What a watch attempt achieved for one indexed tree
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatchOutcome {
    /// Every directory the index covers is registered
    Watching,
    /// The corpus root is gone, so there is nothing to watch
    Unwatchable,
    /// The watch budget cannot cover the tree, so none of it is watched
    Exhausted,
}

#[cfg(not(target_os = "linux"))]
pub struct Watcher;

#[cfg(not(target_os = "linux"))]
impl Watcher {
    pub fn with_budget(_budget: crate::budget::WatchBudget) -> anyhow::Result<Self> {
        Ok(Self)
    }

    pub const fn ceiling(&self) -> usize {
        0
    }

    pub const fn held(&self) -> usize {
        0
    }

    pub const fn spare(&self) -> usize {
        0
    }

    pub const fn watches_tree(&self, _state_root: &std::path::Path) -> bool {
        false
    }

    pub fn watch_tree(
        &mut self,
        _index_root: &std::path::Path,
        _state_root: &std::path::Path,
    ) -> anyhow::Result<WatchOutcome> {
        Ok(WatchOutcome::Unwatchable)
    }

    pub fn watch_dirs(
        &mut self,
        _index_root: &std::path::Path,
        _dirs: &[std::path::PathBuf],
        _state_root: &std::path::Path,
    ) -> anyhow::Result<WatchOutcome> {
        Ok(WatchOutcome::Unwatchable)
    }

    pub fn watch_signal_dir(&mut self, _dir: &std::path::Path) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn watched_trees(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }

    pub const fn release_tree(&mut self, _state_root: &std::path::Path) {}

    pub fn drain_dirty(&mut self) -> anyhow::Result<Vec<std::path::PathBuf>> {
        Ok(Vec::new())
    }

    pub fn wait_dirty(
        &mut self,
        timeout: std::time::Duration,
    ) -> anyhow::Result<Vec<std::path::PathBuf>> {
        std::thread::sleep(timeout);
        Ok(Vec::new())
    }
}
