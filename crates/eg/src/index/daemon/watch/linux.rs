//! Budgeted inotify registration for daemon-maintained index generations.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};

use super::{
    WatchOutcome,
    dirs::{child_dirs, is_state_path},
    events::ParsedEvent,
    inotify::Inotify,
    registered::{Registered, tolerate_unwatchable},
    registry::WatchRegistry,
};
use crate::budget::WatchBudget;

pub struct Watcher {
    inotify: Inotify,
    registry: WatchRegistry,
}

impl Watcher {
    pub fn with_budget(budget: WatchBudget) -> anyhow::Result<Self> {
        Ok(Self {
            inotify: Inotify::open()?,
            registry: WatchRegistry::new(budget),
        })
    }

    pub const fn ceiling(&self) -> usize {
        self.registry.ceiling()
    }

    pub const fn held(&self) -> usize {
        self.registry.held()
    }

    pub const fn spare(&self) -> usize {
        self.registry.spare()
    }

    pub fn watches_tree(&self, state_root: &Path) -> bool {
        self.registry.covers(state_root)
    }

    pub fn watch_tree(
        &mut self,
        index_root: &Path,
        state_root: &Path,
    ) -> anyhow::Result<WatchOutcome> {
        if !index_root.is_dir() {
            return Ok(WatchOutcome::Unwatchable);
        }
        if self.watch_dir_recursive(index_root, state_root)?.is_spent() {
            return Ok(self.give_up_on(state_root));
        }
        Ok(WatchOutcome::Watching)
    }

    /// Watch exactly the walked directories, pruning watches the walk dropped
    pub fn watch_dirs(
        &mut self,
        index_root: &Path,
        dirs: &[PathBuf],
        state_root: &Path,
    ) -> anyhow::Result<WatchOutcome> {
        if !index_root.is_dir() {
            return Ok(WatchOutcome::Unwatchable);
        }
        self.prune_watches(index_root, dirs, state_root);
        let wanted = || std::iter::once(index_root).chain(dirs.iter().map(PathBuf::as_path));
        if self.registry.unwatched_count(wanted()) > self.registry.spare() {
            return Ok(self.give_up_on(state_root));
        }
        for dir in wanted() {
            if self.watch_one_dir(dir, Some(state_root))?.is_spent() {
                return Ok(self.give_up_on(state_root));
            }
        }
        Ok(WatchOutcome::Watching)
    }

    /// Watch a coordination directory whose events only wake the poll
    pub fn watch_signal_dir(&mut self, dir: &Path) -> anyhow::Result<()> {
        self.watch_one_dir(dir, None)?;
        Ok(())
    }

    /// Trees this watcher currently reports changes for
    pub fn watched_trees(&self) -> Vec<PathBuf> {
        self.registry.state_roots()
    }

    /// Drop every watch held for one tree and return its budget
    pub fn release_tree(&mut self, state_root: &Path) {
        for wd in self.registry.all_for(state_root) {
            self.drop_watch(wd);
        }
    }

    /// Release a tree we cannot cover completely; a partial watch is not sound
    fn give_up_on(&mut self, state_root: &Path) -> WatchOutcome {
        self.release_tree(state_root);
        WatchOutcome::Exhausted
    }

    fn prune_watches(&mut self, index_root: &Path, dirs: &[PathBuf], state_root: &Path) {
        let keep: HashSet<&Path> = dirs
            .iter()
            .map(PathBuf::as_path)
            .chain([index_root])
            .collect();
        for wd in self.registry.stale_for(state_root, &keep) {
            self.drop_watch(wd);
        }
    }

    fn drop_watch(&mut self, wd: i32) {
        if self.registry.remove(wd).is_some() {
            self.inotify.remove(wd);
        }
    }

    pub fn drain_dirty(&mut self) -> anyhow::Result<Vec<PathBuf>> {
        let mut dirty = HashSet::new();
        let mut buffer = vec![0u8; 64 * 1024];
        while let Some(len) = self.inotify.read(&mut buffer)? {
            self.record_events(&buffer[..len], &mut dirty)?;
        }
        Ok(dirty.into_iter().collect())
    }

    pub fn wait_dirty(&mut self, timeout: Duration) -> anyhow::Result<Vec<PathBuf>> {
        if self.inotify.wait(timeout)? {
            return self.drain_dirty();
        }
        Ok(Vec::new())
    }

    fn watch_dir_recursive(
        &mut self,
        root: &Path,
        state_root: &Path,
    ) -> anyhow::Result<Registered> {
        if is_state_path(root, state_root) {
            return Ok(Registered::Skipped);
        }
        if self.watch_one_dir(root, Some(state_root))?.is_spent() {
            return Ok(Registered::Exhausted);
        }
        for path in child_dirs(root, state_root)? {
            if self.watch_dir_recursive(&path, state_root)?.is_spent() {
                return Ok(Registered::Exhausted);
            }
        }
        Ok(Registered::Added)
    }

    fn watch_one_dir(
        &mut self,
        dir: &Path,
        state_root: Option<&Path>,
    ) -> anyhow::Result<Registered> {
        if self.registry.is_watched(dir) {
            return Ok(Registered::Skipped);
        }
        if !self.registry.claim() {
            return Ok(Registered::Exhausted);
        }
        match self.inotify.add(dir) {
            Ok(wd) => {
                self.registry.insert(wd, dir, state_root);
                Ok(Registered::Added)
            },
            Err(err) => {
                self.registry.refund();
                tolerate_unwatchable(err)
            },
        }
    }

    fn record_events(
        &mut self,
        mut bytes: &[u8],
        dirty: &mut HashSet<PathBuf>,
    ) -> anyhow::Result<()> {
        while let Some((event, rest)) = ParsedEvent::take(bytes)? {
            self.apply_event(&event, dirty)?;
            bytes = rest;
        }
        Ok(())
    }

    fn apply_event(
        &mut self,
        event: &ParsedEvent,
        dirty: &mut HashSet<PathBuf>,
    ) -> anyhow::Result<()> {
        let Some(watched) = self.registry.watched(event.wd()) else {
            return Ok(());
        };
        let Some(state_root) = watched.state_root().map(Path::to_path_buf) else {
            return Ok(());
        };
        let path = watched.event_path(event.name());
        if is_state_path(&path, &state_root) {
            return Ok(());
        }
        dirty.insert(state_root.clone());
        self.grow_into(event, &path, &state_root)
    }

    /// Follow a newly created directory, dropping the tree if it will not fit
    fn grow_into(
        &mut self,
        event: &ParsedEvent,
        path: &Path,
        state_root: &Path,
    ) -> anyhow::Result<()> {
        if !event.created_dir() {
            return Ok(());
        }
        if self.watch_dir_recursive(path, state_root)?.is_spent() {
            self.give_up_on(state_root);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{WatchOutcome, Watcher};
    use crate::budget::WatchBudget;
    use std::{fs, path::Path, time::Duration};

    fn scratch(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("eg-watch-{name}-"))
            .tempdir()
            .expect("scratch dir")
    }

    fn budgeted(ceiling: usize) -> Watcher {
        Watcher::with_budget(WatchBudget::with_ceiling(ceiling)).expect("watcher")
    }

    /// Corpus root with a state dir and `depth` nested subdirectories
    fn corpus(root: &Path, depth: usize) -> std::path::PathBuf {
        let state = root.join(".eg");
        fs::create_dir_all(&state).expect("state");
        let mut nested = root.to_path_buf();
        for level in 0..depth {
            nested = nested.join(format!("level{level}"));
        }
        fs::create_dir_all(&nested).expect("nested");
        state
    }

    #[test]
    fn file_event_marks_state_root_dirty() {
        let root_guard = scratch("dirty");
        let root = root_guard.path().to_path_buf();
        let state = corpus(&root, 0);

        let mut watcher = budgeted(64);
        assert!(matches!(
            watcher.watch_tree(&root, &state).expect("watch tree"),
            WatchOutcome::Watching
        ));
        fs::write(root.join("changed.txt"), "changed").expect("write");
        std::thread::sleep(Duration::from_millis(20));

        assert!(watcher.drain_dirty().expect("dirty").contains(&state));
    }

    #[test]
    fn missing_root_is_skipped_without_error() {
        let root_guard = scratch("missing");
        let gone = root_guard.path().join("deleted-corpus");
        let state = gone.join(".eg");
        let mut watcher = budgeted(64);

        assert!(matches!(
            watcher.watch_tree(&gone, &state).expect("must not error"),
            WatchOutcome::Unwatchable
        ));
    }

    #[test]
    fn state_root_events_are_ignored() {
        let root_guard = scratch("state");
        let root = root_guard.path().to_path_buf();
        let state = corpus(&root, 0);
        let nested_state = root.join("src/.eg");
        fs::create_dir_all(&nested_state).expect("nested state");
        let mut watcher = budgeted(64);
        watcher.watch_tree(&root, &state).expect("watch tree");
        fs::write(state.join("runtime-marker"), "ignored").expect("write");
        fs::write(nested_state.join("runtime-marker"), "ignored").expect("write");
        std::thread::sleep(Duration::from_millis(20));

        assert!(watcher.drain_dirty().expect("dirty").is_empty());
    }

    #[test]
    fn a_tree_over_budget_is_refused_whole() {
        let root_guard = scratch("over-budget");
        let root = root_guard.path().to_path_buf();
        let state = corpus(&root, 6);

        let mut watcher = budgeted(4);
        let outcome = watcher.watch_tree(&root, &state).expect("watch tree");

        assert!(matches!(outcome, WatchOutcome::Exhausted));
        assert_eq!(0, watcher.held());
        assert!(watcher.watched_trees().is_empty());
        assert!(!watcher.watches_tree(&state));
    }

    #[test]
    fn a_refused_tree_leaves_earlier_trees_watched() {
        let root_guard = scratch("first-come");
        let root = root_guard.path().to_path_buf();
        let small = root.join("small");
        let large = root.join("large");
        let small_state = corpus(&small, 1);
        let large_state = corpus(&large, 8);

        let mut watcher = budgeted(5);
        let kept = watcher.watch_tree(&small, &small_state).expect("small");
        let refused = watcher.watch_tree(&large, &large_state).expect("large");

        assert!(matches!(kept, WatchOutcome::Watching));
        assert!(matches!(refused, WatchOutcome::Exhausted));
        assert!(watcher.watches_tree(&small_state));
        assert!(!watcher.watches_tree(&large_state));

        fs::write(small.join("changed.txt"), "changed").expect("write");
        std::thread::sleep(Duration::from_millis(20));
        assert!(watcher.drain_dirty().expect("dirty").contains(&small_state));
    }

    #[test]
    fn a_directory_set_over_budget_is_refused_whole() {
        let root_guard = scratch("dirs-over-budget");
        let root = root_guard.path().to_path_buf();
        let state = corpus(&root, 0);
        let dirs: Vec<std::path::PathBuf> = (0..8)
            .map(|index| {
                let dir = root.join(format!("dir{index}"));
                fs::create_dir_all(&dir).expect("dir");
                dir
            })
            .collect();

        let mut watcher = budgeted(4);
        let outcome = watcher
            .watch_dirs(&root, &dirs, &state)
            .expect("watch dirs");

        assert!(matches!(outcome, WatchOutcome::Exhausted));
        assert_eq!(0, watcher.held());
    }

    #[test]
    fn releasing_a_tree_returns_its_budget() {
        let root_guard = scratch("release");
        let root = root_guard.path().to_path_buf();
        let state = corpus(&root, 3);

        let mut watcher = budgeted(64);
        watcher.watch_tree(&root, &state).expect("watch tree");
        assert!(watcher.held() > 0);

        watcher.release_tree(&state);

        assert_eq!(0, watcher.held());
        assert!(watcher.watched_trees().is_empty());
    }

    #[test]
    fn a_new_subdirectory_that_will_not_fit_releases_the_tree() {
        let root_guard = scratch("grow");
        let root = root_guard.path().to_path_buf();
        let state = corpus(&root, 0);

        let mut watcher = budgeted(1);
        assert!(matches!(
            watcher.watch_tree(&root, &state).expect("watch tree"),
            WatchOutcome::Watching
        ));
        fs::create_dir_all(root.join("late")).expect("late dir");
        std::thread::sleep(Duration::from_millis(20));

        assert!(watcher.drain_dirty().expect("dirty").contains(&state));
        assert!(!watcher.watches_tree(&state));
        assert_eq!(0, watcher.held());
    }
}
