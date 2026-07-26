//! Bookkeeping for the directories one watcher currently holds.

use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use crate::budget::WatchBudget;

/// One directory registered with inotify, and the tree it reports for
#[derive(Clone)]
pub struct WatchedDir {
    dir: PathBuf,
    state_root: Option<PathBuf>,
}

impl WatchedDir {
    pub fn state_root(&self) -> Option<&Path> {
        self.state_root.as_deref()
    }

    /// Path an event names, or the watched directory for a self event
    pub fn event_path(&self, name: &[u8]) -> PathBuf {
        let name = name.split(|byte| *byte == 0).next().unwrap_or_default();
        if name.is_empty() {
            return self.dir.clone();
        }
        self.dir.join(OsStr::from_bytes(name))
    }
}

/// Watch descriptors held by one watcher, bounded by a claim budget
pub struct WatchRegistry {
    dirs_by_watch: HashMap<i32, WatchedDir>,
    watched_dirs: HashSet<PathBuf>,
    budget: WatchBudget,
}

impl WatchRegistry {
    pub fn new(budget: WatchBudget) -> Self {
        Self {
            dirs_by_watch: HashMap::new(),
            watched_dirs: HashSet::new(),
            budget,
        }
    }

    pub const fn ceiling(&self) -> usize {
        self.budget.ceiling()
    }

    pub const fn held(&self) -> usize {
        self.budget.held()
    }

    pub const fn spare(&self) -> usize {
        self.budget.spare()
    }

    pub fn is_watched(&self, dir: &Path) -> bool {
        self.watched_dirs.contains(dir)
    }

    /// Count directories in `dirs` that are not registered yet
    pub fn unwatched_count<'a>(&self, dirs: impl Iterator<Item = &'a Path>) -> usize {
        dirs.filter(|dir| !self.is_watched(dir)).count()
    }

    pub const fn claim(&mut self) -> bool {
        self.budget.claim()
    }

    pub const fn refund(&mut self) {
        self.budget.release(1);
    }

    /// Record a claimed watch, refunding any path spelling it displaces
    pub fn insert(&mut self, wd: i32, dir: &Path, state_root: Option<&Path>) {
        self.watched_dirs.insert(dir.to_path_buf());
        let watched = WatchedDir {
            dir: dir.to_path_buf(),
            state_root: state_root.map(Path::to_path_buf),
        };
        if let Some(displaced) = self.dirs_by_watch.insert(wd, watched) {
            self.watched_dirs.remove(&displaced.dir);
            self.budget.release(1);
        }
    }

    pub fn remove(&mut self, wd: i32) -> Option<WatchedDir> {
        let watched = self.dirs_by_watch.remove(&wd)?;
        self.watched_dirs.remove(&watched.dir);
        self.budget.release(1);
        Some(watched)
    }

    pub fn watched(&self, wd: i32) -> Option<WatchedDir> {
        self.dirs_by_watch.get(&wd).cloned()
    }

    /// Watch descriptors for one tree that the walk no longer covers
    pub fn stale_for(&self, state_root: &Path, keep: &HashSet<&Path>) -> Vec<i32> {
        self.watches_matching(state_root, |watched| !keep.contains(watched.dir.as_path()))
    }

    /// Every watch descriptor registered for one tree
    pub fn all_for(&self, state_root: &Path) -> Vec<i32> {
        self.watches_matching(state_root, |_| true)
    }

    /// True when at least one watch still reports for this tree
    pub fn covers(&self, state_root: &Path) -> bool {
        self.dirs_by_watch
            .values()
            .any(|watched| watched.state_root.as_deref() == Some(state_root))
    }

    /// Trees this registry currently reports events for
    pub fn state_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = self
            .dirs_by_watch
            .values()
            .filter_map(|watched| watched.state_root.clone())
            .collect();
        roots.sort_unstable();
        roots.dedup();
        roots
    }

    fn watches_matching(&self, state_root: &Path, keep: impl Fn(&WatchedDir) -> bool) -> Vec<i32> {
        self.dirs_by_watch
            .iter()
            .filter(|(_, watched)| watched.state_root.as_deref() == Some(state_root))
            .filter(|(_, watched)| keep(watched))
            .map(|(wd, _)| *wd)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::WatchRegistry;
    use crate::budget::WatchBudget;
    use std::{collections::HashSet, path::Path};

    fn registry(ceiling: usize) -> WatchRegistry {
        WatchRegistry::new(WatchBudget::with_ceiling(ceiling))
    }

    #[test]
    fn inserting_and_removing_tracks_the_budget() {
        let mut registry = registry(2);
        assert!(registry.claim());
        registry.insert(1, Path::new("/repo/src"), Some(Path::new("/repo/.eg")));

        assert!(registry.is_watched(Path::new("/repo/src")));
        assert_eq!(1, registry.held());

        registry.remove(1);

        assert!(!registry.is_watched(Path::new("/repo/src")));
        assert_eq!(0, registry.held());
    }

    /// Two paths can reach one directory, and the kernel charges one watch
    #[test]
    fn a_displaced_path_refunds_its_claim() {
        let mut registry = registry(4);
        assert!(registry.claim());
        registry.insert(1, Path::new("/repo/src"), Some(Path::new("/repo/.eg")));
        assert!(registry.claim());
        registry.insert(
            1,
            Path::new("/repo/linked-src"),
            Some(Path::new("/repo/.eg")),
        );

        assert_eq!(1, registry.held());
        assert!(!registry.is_watched(Path::new("/repo/src")));
        assert!(registry.is_watched(Path::new("/repo/linked-src")));
    }

    #[test]
    fn unwatched_count_ignores_registered_dirs() {
        let mut registry = registry(4);
        assert!(registry.claim());
        registry.insert(1, Path::new("/repo/src"), Some(Path::new("/repo/.eg")));

        let dirs = [Path::new("/repo/src"), Path::new("/repo/docs")];

        assert_eq!(1, registry.unwatched_count(dirs.into_iter()));
    }

    #[test]
    fn stale_and_all_select_by_tree() {
        let mut registry = registry(8);
        for (wd, dir, state) in [
            (1, "/a/src", "/a/.eg"),
            (2, "/a/docs", "/a/.eg"),
            (3, "/b/src", "/b/.eg"),
        ] {
            assert!(registry.claim());
            registry.insert(wd, Path::new(dir), Some(Path::new(state)));
        }

        let keep: HashSet<&Path> = std::iter::once(Path::new("/a/src")).collect();
        assert_eq!(vec![2], registry.stale_for(Path::new("/a/.eg"), &keep));
        assert_eq!(vec![3], registry.all_for(Path::new("/b/.eg")));
        assert_eq!(
            vec![Path::new("/a/.eg"), Path::new("/b/.eg")],
            registry.state_roots()
        );
    }

    #[test]
    fn signal_dirs_belong_to_no_tree() {
        let mut registry = registry(2);
        assert!(registry.claim());
        registry.insert(1, Path::new("/run/eg/requests"), None);

        assert!(registry.state_roots().is_empty());
        assert!(registry.all_for(Path::new("/repo/.eg")).is_empty());
    }
}
