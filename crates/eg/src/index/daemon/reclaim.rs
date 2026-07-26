//! Reclamation of index state no daemon serves any more.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use super::generations;

const INDEX_DIR_NAME: &str = "index";
const RUNTIME_DIR_NAME: &str = "runtime";
const REQUESTS_DIR_NAME: &str = "requests";
const GITIGNORE_NAME: &str = ".gitignore";
const GITIGNORE_BODY: &str = "*\n";
const QUARANTINE_INFIX: &str = ".bad-";
const QUARANTINE_TTL: Duration = Duration::from_hours(24);

/// One index state root the daemon knows from its request directory.
pub struct KnownRoot {
    pub index_root: PathBuf,
    pub state_root: PathBuf,
    pub lease_live: bool,
}

/// What one sweep freed.
#[derive(Default)]
pub struct Reclaimed {
    pub bytes: u64,
    pub paths: usize,
}

impl Reclaimed {
    pub const fn absorb(&mut self, other: &Self) {
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.paths += other.paths;
    }

    /// Delete one path, counting the bytes it held
    pub fn take(&mut self, path: &Path) {
        let bytes = path_bytes(path);
        if remove_path(path) {
            self.bytes = self.bytes.saturating_add(bytes);
            self.paths += 1;
        }
    }
}

/// Reclaim index state the daemon knows the paths for but no longer serves.
pub fn sweep(roots: &[KnownRoot]) -> Reclaimed {
    let mut reclaimed = Reclaimed::default();
    for root in roots {
        if root.index_root.is_dir() {
            let index_dir = root.state_root.join(INDEX_DIR_NAME);
            reclaimed.absorb(&generations::sweep(&index_dir, root.lease_live));
        } else {
            reclaimed.absorb(&abandon_state_root(&root.state_root, &root.index_root));
        }
    }
    reclaimed
}

/// Drop quarantined request files old enough that no daemon will read them.
pub fn sweep_quarantine(runtime_root: &Path) -> Reclaimed {
    let mut reclaimed = Reclaimed::default();
    let Ok(entries) = fs::read_dir(runtime_root.join(REQUESTS_DIR_NAME)) else {
        return reclaimed;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry
            .file_name()
            .to_string_lossy()
            .contains(QUARANTINE_INFIX)
        {
            continue;
        }
        if older_than(&path, QUARANTINE_TTL) {
            reclaimed.take(&path);
        }
    }
    reclaimed
}

/// Remove the state shell an index kept outside a corpus that is now gone.
fn abandon_state_root(state_root: &Path, index_root: &Path) -> Reclaimed {
    let mut reclaimed = Reclaimed::default();
    if state_root.starts_with(index_root) {
        return reclaimed;
    }
    reclaimed.take(&state_root.join(INDEX_DIR_NAME));
    reclaimed.take(&state_root.join(RUNTIME_DIR_NAME));
    remove_own_gitignore(state_root);
    let _ = fs::remove_dir(state_root);
    reclaimed
}

/// Delete only the ignore file eg wrote itself.
fn remove_own_gitignore(state_root: &Path) {
    let path = state_root.join(GITIGNORE_NAME);
    if fs::read_to_string(&path).is_ok_and(|body| body == GITIGNORE_BODY) {
        let _ = fs::remove_file(path);
    }
}

fn older_than(path: &Path, ttl: Duration) -> bool {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > ttl)
}

fn remove_path(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path).is_ok(),
        Ok(_) => fs::remove_file(path).is_ok(),
        Err(_) => false,
    }
}

fn path_bytes(path: &Path) -> u64 {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !metadata.is_dir() {
        return metadata.len();
    }
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| path_bytes(&entry.path()))
        .fold(0, u64::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::{KnownRoot, QUARANTINE_TTL, sweep, sweep_quarantine};
    use crate::generations::POSTINGS_GENERATION;
    use std::{
        fs,
        path::Path,
        time::{Duration, SystemTime},
    };

    fn scratch(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("eg-reclaim-{name}-"))
            .tempdir()
            .expect("scratch dir")
    }

    fn generation(index_dir: &Path, bytes: usize) {
        let dir = index_dir.join(POSTINGS_GENERATION);
        fs::create_dir_all(&dir).expect("generation");
        fs::write(dir.join("manifest.bin"), vec![b'x'; bytes]).expect("manifest");
    }

    #[test]
    fn cache_state_root_of_a_deleted_corpus_is_removed() {
        let root_guard = scratch("abandoned");
        let root = root_guard.path().to_path_buf();
        let state_root = root.join("cache/abcd");
        generation(&state_root.join("index"), 128);
        fs::create_dir_all(state_root.join("runtime")).expect("runtime");
        fs::write(state_root.join("runtime/lease"), "lease").expect("lease");
        fs::write(state_root.join(".gitignore"), "*\n").expect("gitignore");

        let reclaimed = sweep(&[KnownRoot {
            index_root: root.join("gone"),
            state_root: state_root.clone(),
            lease_live: false,
        }]);

        assert_eq!(2, reclaimed.paths);
        assert!(reclaimed.bytes >= 128);
        assert!(!state_root.exists());
    }

    #[test]
    fn local_state_of_a_deleted_corpus_needs_no_sweep() {
        let root_guard = scratch("local-gone");
        let root = root_guard.path().to_path_buf();
        let index_root = root.join("corpus");
        let state_root = index_root.join(".eg");

        let reclaimed = sweep(&[KnownRoot {
            index_root,
            state_root,
            lease_live: false,
        }]);

        assert_eq!(0, reclaimed.paths);
    }

    #[test]
    fn quarantined_requests_expire_but_fresh_ones_stay() {
        let root_guard = scratch("quarantine");
        let root = root_guard.path().to_path_buf();
        let requests = root.join("requests");
        fs::create_dir_all(&requests).expect("requests");
        let stale = requests.join("a.request.bad-1");
        let fresh = requests.join("b.request.bad-2");
        fs::write(&stale, "stale").expect("stale");
        fs::write(&fresh, "fresh").expect("fresh");
        let old = SystemTime::now() - QUARANTINE_TTL - Duration::from_mins(1);
        fs::File::open(&stale)
            .expect("open")
            .set_modified(old)
            .expect("backdate");

        let reclaimed = sweep_quarantine(&root);

        assert_eq!(1, reclaimed.paths);
        assert!(!stale.exists());
        assert!(fresh.exists());
    }
}
