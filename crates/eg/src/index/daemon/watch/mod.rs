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

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

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

/// Paths one tree changed since the last drain
#[derive(Default)]
pub struct TreeChanges {
    paths: Vec<PathBuf>,
    coarse: bool,
}

impl TreeChanges {
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// True when the change set is wider than the paths named here
    pub const fn is_coarse(&self) -> bool {
        self.coarse
    }
}

/// One drain of the watcher, grouped by the tree each path belongs to
#[derive(Default)]
pub struct DirtyEvents {
    trees: HashMap<PathBuf, TreeChanges>,
}

impl DirtyEvents {
    /// Record one changed path, widening the tree when the change is coarse
    pub fn record(&mut self, state_root: &Path, path: PathBuf, coarse: bool) {
        let changes = self.tree(state_root);
        changes.paths.push(path);
        changes.coarse |= coarse;
    }

    /// Widen every named tree, for events that name no path of their own
    pub fn widen(&mut self, state_roots: &[PathBuf]) {
        for state_root in state_roots {
            self.tree(state_root).coarse = true;
        }
    }

    pub fn into_trees(self) -> impl Iterator<Item = (PathBuf, TreeChanges)> {
        self.trees.into_iter()
    }

    fn tree(&mut self, state_root: &Path) -> &mut TreeChanges {
        self.trees.entry(state_root.to_path_buf()).or_default()
    }
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

    pub fn drain_dirty(&mut self) -> anyhow::Result<DirtyEvents> {
        Ok(DirtyEvents::default())
    }

    pub fn wait_dirty(&mut self, timeout: std::time::Duration) -> anyhow::Result<DirtyEvents> {
        std::thread::sleep(timeout);
        Ok(DirtyEvents::default())
    }
}
