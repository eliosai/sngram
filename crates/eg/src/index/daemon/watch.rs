//! Filesystem invalidation for daemon-maintained index generations.

#[cfg(target_os = "linux")]
pub use linux::Watcher;

#[cfg(not(target_os = "linux"))]
pub struct Watcher;

#[cfg(not(target_os = "linux"))]
impl Watcher {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self)
    }

    pub fn watch_tree(
        &mut self,
        _index_root: &std::path::Path,
        _state_root: &std::path::Path,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }

    pub fn watch_dirs(
        &mut self,
        _index_root: &std::path::Path,
        _dirs: &[std::path::PathBuf],
        _state_root: &std::path::Path,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }

    pub fn watch_signal_dir(&mut self, _dir: &std::path::Path) -> anyhow::Result<()> {
        Ok(())
    }

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

#[cfg(target_os = "linux")]
mod linux {
    #![allow(unsafe_code, reason = "Linux inotify is exposed through libc FFI")]

    use std::{
        collections::{HashMap, HashSet},
        ffi::OsStr,
        fs, io, mem,
        os::{fd::RawFd, unix::ffi::OsStrExt},
        path::{Path, PathBuf},
        time::Duration,
    };

    const WATCH_MASK: u32 = libc::IN_ATTRIB
        | libc::IN_CLOSE_WRITE
        | libc::IN_CREATE
        | libc::IN_DELETE
        | libc::IN_DELETE_SELF
        | libc::IN_MODIFY
        | libc::IN_MOVE_SELF
        | libc::IN_MOVED_FROM
        | libc::IN_MOVED_TO;

    pub struct Watcher {
        fd: RawFd,
        dirs_by_watch: HashMap<i32, WatchedDir>,
        watched_dirs: HashSet<PathBuf>,
    }

    impl Watcher {
        pub fn new() -> anyhow::Result<Self> {
            let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
            if fd < 0 {
                return Err(io::Error::last_os_error().into());
            }
            Ok(Self {
                fd,
                dirs_by_watch: HashMap::new(),
                watched_dirs: HashSet::new(),
            })
        }

        pub fn watch_tree(&mut self, index_root: &Path, state_root: &Path) -> anyhow::Result<bool> {
            if !index_root.is_dir() {
                return Ok(false);
            }
            self.watch_dir_recursive(index_root, state_root)?;
            Ok(true)
        }

        /// Watch exactly the walked directories, pruning watches the walk dropped
        pub fn watch_dirs(
            &mut self,
            index_root: &Path,
            dirs: &[PathBuf],
            state_root: &Path,
        ) -> anyhow::Result<bool> {
            if !index_root.is_dir() {
                return Ok(false);
            }
            self.watch_one_dir(index_root, Some(state_root))?;
            for dir in dirs {
                self.watch_one_dir(dir, Some(state_root))?;
            }
            self.prune_watches(index_root, dirs, state_root);
            Ok(true)
        }

        /// Watch a coordination directory whose events only wake the poll
        pub fn watch_signal_dir(&mut self, dir: &Path) -> anyhow::Result<()> {
            self.watch_one_dir(dir, None)
        }

        fn prune_watches(&mut self, index_root: &Path, dirs: &[PathBuf], state_root: &Path) {
            let keep: HashSet<&Path> = dirs
                .iter()
                .map(PathBuf::as_path)
                .chain([index_root])
                .collect();
            let stale: Vec<i32> = self
                .dirs_by_watch
                .iter()
                .filter(|(_, watched)| {
                    watched.state_root.as_deref() == Some(state_root)
                        && !keep.contains(watched.dir.as_path())
                })
                .map(|(wd, _)| *wd)
                .collect();
            for wd in stale {
                self.drop_watch(wd);
            }
        }

        fn drop_watch(&mut self, wd: i32) {
            let Some(watched) = self.dirs_by_watch.remove(&wd) else {
                return;
            };
            self.watched_dirs.remove(&watched.dir);
            unsafe { libc::inotify_rm_watch(self.fd, wd) };
        }

        pub fn drain_dirty(&mut self) -> anyhow::Result<Vec<PathBuf>> {
            let mut dirty = HashSet::new();
            let mut buffer = vec![0u8; 64 * 1024];
            while let Some(len) = self.read_events(&mut buffer)? {
                self.record_events(&buffer[..usize::try_from(len)?], &mut dirty)?;
            }
            Ok(dirty.into_iter().collect())
        }

        pub fn wait_dirty(&mut self, timeout: Duration) -> anyhow::Result<Vec<PathBuf>> {
            if self.wait_for_event(timeout)? {
                return self.drain_dirty();
            }
            Ok(Vec::new())
        }

        fn wait_for_event(&self, timeout: Duration) -> anyhow::Result<bool> {
            let mut pollfd = libc::pollfd {
                fd: self.fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout = timeout.as_millis().try_into().unwrap_or(i32::MAX);
            let ready = unsafe { libc::poll(&raw mut pollfd, 1, timeout) };
            match ready.cmp(&0) {
                std::cmp::Ordering::Greater => Ok(pollfd.revents & libc::POLLIN != 0),
                std::cmp::Ordering::Equal => Ok(false),
                std::cmp::Ordering::Less => Err(io::Error::last_os_error().into()),
            }
        }

        fn read_events(&self, buffer: &mut [u8]) -> anyhow::Result<Option<isize>> {
            let len = unsafe {
                libc::read(
                    self.fd,
                    buffer.as_mut_ptr().cast::<libc::c_void>(),
                    buffer.len(),
                )
            };
            match len.cmp(&0) {
                std::cmp::Ordering::Greater => Ok(Some(len)),
                std::cmp::Ordering::Equal => Ok(None),
                std::cmp::Ordering::Less => Self::read_error(),
            }
        }

        fn read_error() -> anyhow::Result<Option<isize>> {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            Err(err.into())
        }

        fn watch_dir_recursive(&mut self, root: &Path, state_root: &Path) -> anyhow::Result<()> {
            if is_state_path(root, state_root) {
                return Ok(());
            }
            self.watch_one_dir(root, Some(state_root))?;
            for path in child_dirs(root, state_root)? {
                self.watch_dir_recursive(&path, state_root)?;
            }
            Ok(())
        }

        fn watch_one_dir(&mut self, dir: &Path, state_root: Option<&Path>) -> anyhow::Result<()> {
            if !self.watched_dirs.insert(dir.to_path_buf()) {
                return Ok(());
            }
            let path = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
            let wd = unsafe { libc::inotify_add_watch(self.fd, path.as_ptr(), WATCH_MASK) };
            if wd < 0 {
                self.watched_dirs.remove(dir);
                return tolerate_unwatchable(io::Error::last_os_error());
            }
            self.dirs_by_watch.insert(
                wd,
                WatchedDir {
                    dir: dir.to_path_buf(),
                    state_root: state_root.map(Path::to_path_buf),
                },
            );
            Ok(())
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
            let Some(watched) = self.dirs_by_watch.get(&event.wd).cloned() else {
                return Ok(());
            };
            let Some(state_root) = watched.state_root.as_deref() else {
                return Ok(());
            };
            let path = watched.event_path(&event.name);
            if is_state_path(&path, state_root) {
                return Ok(());
            }
            dirty.insert(state_root.to_path_buf());
            if event.created_dir() {
                self.watch_dir_recursive(&path, state_root)?;
            }
            Ok(())
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            let _ = unsafe { libc::close(self.fd) };
        }
    }

    #[derive(Clone)]
    struct WatchedDir {
        dir: PathBuf,
        state_root: Option<PathBuf>,
    }

    impl WatchedDir {
        fn event_path(&self, name: &[u8]) -> PathBuf {
            let name = name.split(|byte| *byte == 0).next().unwrap_or_default();
            if name.is_empty() {
                return self.dir.clone();
            }
            self.dir.join(OsStr::from_bytes(name))
        }
    }

    struct ParsedEvent {
        wd: i32,
        mask: u32,
        name: Vec<u8>,
    }

    impl ParsedEvent {
        fn take(bytes: &[u8]) -> anyhow::Result<Option<(Self, &[u8])>> {
            let header_len = mem::size_of::<libc::inotify_event>();
            if bytes.len() < header_len {
                return Ok(None);
            }
            let event = unsafe {
                bytes
                    .as_ptr()
                    .cast::<libc::inotify_event>()
                    .read_unaligned()
            };
            let total_len = header_len.saturating_add(usize::try_from(event.len)?);
            if bytes.len() < total_len {
                return Ok(None);
            }
            let parsed = Self {
                wd: event.wd,
                mask: event.mask,
                name: bytes[header_len..total_len].to_vec(),
            };
            Ok(Some((parsed, &bytes[total_len..])))
        }

        const fn created_dir(&self) -> bool {
            self.mask & libc::IN_ISDIR != 0
                && self.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0
        }
    }

    fn child_dirs(root: &Path, state_root: &Path) -> anyhow::Result<Vec<PathBuf>> {
        let Ok(entries) = fs::read_dir(root) else {
            return Ok(Vec::new());
        };
        let mut dirs = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if !is_state_path(&path, state_root) && entry.file_type().is_ok_and(|ty| ty.is_dir()) {
                dirs.push(path);
            }
        }
        Ok(dirs)
    }

    fn tolerate_unwatchable(err: io::Error) -> anyhow::Result<()> {
        match err.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied => Ok(()),
            _ => Err(err.into()),
        }
    }

    fn is_state_path(path: &Path, state_root: &Path) -> bool {
        path == state_root
            || path.starts_with(state_root)
            || path
                .components()
                .any(|component| component.as_os_str() == ".eg")
    }

    #[cfg(test)]
    mod tests {
        use super::Watcher;
        use std::{fs, time::Duration};

        fn scratch(name: &str) -> tempfile::TempDir {
            tempfile::Builder::new()
                .prefix(&format!("eg-watch-{name}-"))
                .tempdir()
                .expect("scratch dir")
        }

        #[test]
        fn file_event_marks_state_root_dirty() {
            let root_guard = scratch("dirty");
            let root = root_guard.path().to_path_buf();
            let state = root.join(".eg");
            fs::create_dir_all(&state).expect("state");

            let mut watcher = Watcher::new().expect("watcher");
            assert!(watcher.watch_tree(&root, &state).expect("watch tree"));
            fs::write(root.join("changed.txt"), "changed").expect("write");
            std::thread::sleep(Duration::from_millis(20));

            let dirty = watcher.drain_dirty().expect("dirty");
            assert!(dirty.contains(&state));
        }

        #[test]
        fn missing_root_is_skipped_without_error() {
            let root_guard = scratch("missing");
            let root = root_guard.path().to_path_buf();
            let gone = root.join("deleted-corpus");
            let state = gone.join(".eg");

            let mut watcher = Watcher::new().expect("watcher");
            let watching = watcher.watch_tree(&gone, &state).expect("must not error");
            assert!(!watching);
        }

        #[test]
        fn existing_tree_reports_watching() {
            let root_guard = scratch("existing");
            let root = root_guard.path().to_path_buf();
            let state = root.join(".eg");
            fs::create_dir_all(&state).expect("state");
            fs::create_dir_all(root.join("kept")).expect("kept");

            let mut watcher = Watcher::new().expect("watcher");
            assert!(watcher.watch_tree(&root, &state).expect("watch tree"));
        }

        #[test]
        fn state_root_events_are_ignored() {
            let root_guard = scratch("state");
            let root = root_guard.path().to_path_buf();
            let state = root.join(".eg");
            fs::create_dir_all(&state).expect("state");

            let mut watcher = Watcher::new().expect("watcher");
            assert!(watcher.watch_tree(&root, &state).expect("watch tree"));
            fs::write(state.join("runtime-marker"), "ignored").expect("write");
            std::thread::sleep(Duration::from_millis(20));

            assert!(watcher.drain_dirty().expect("dirty").is_empty());
        }

        #[test]
        fn nested_state_root_events_are_ignored() {
            let root_guard = scratch("nested-state");
            let root = root_guard.path().to_path_buf();
            let state = root.join(".eg");
            let nested_state = root.join("src/.eg");
            fs::create_dir_all(&state).expect("state");
            fs::create_dir_all(&nested_state).expect("nested state");

            let mut watcher = Watcher::new().expect("watcher");
            assert!(watcher.watch_tree(&root, &state).expect("watch tree"));
            fs::write(nested_state.join("runtime-marker"), "ignored").expect("write");
            std::thread::sleep(Duration::from_millis(20));

            assert!(watcher.drain_dirty().expect("dirty").is_empty());
        }
    }
}
