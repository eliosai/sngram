//! Paths changed since the published generation, republished for queries.

use std::{
    collections::{BTreeSet, HashMap},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::SystemTime,
};

use super::watch::TreeChanges;

const RUNTIME_DIR_NAME: &str = "runtime";
pub const DIRTY_FILE_NAME: &str = "dirty-paths";
pub const UNBOUNDED_MARKER: &str = "unbounded";
/// Changed paths past which naming them costs more than rescanning the tree
const DIRTY_CEILING: usize = 1024;

/// Paths a tree changed, or a change too wide to name path by path
enum Changed {
    Paths(BTreeSet<PathBuf>),
    Unbounded,
}

impl Default for Changed {
    fn default() -> Self {
        Self::Paths(BTreeSet::new())
    }
}

impl Changed {
    fn add(&mut self, path: &Path) {
        let Self::Paths(paths) = self else {
            return;
        };
        paths.insert(path.to_path_buf());
        if paths.len() > DIRTY_CEILING {
            *self = Self::Unbounded;
        }
    }

    fn widen(&mut self) {
        *self = Self::Unbounded;
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::Paths(paths) => paths.is_empty(),
            Self::Unbounded => false,
        }
    }

    fn absorb(&mut self, changes: &TreeChanges) {
        if changes.is_coarse() {
            self.widen();
            return;
        }
        for path in changes.paths() {
            self.add(path);
        }
    }
}

/// One tree's change set against its published generation and its running build
struct TreeDirt {
    since_publish: Changed,
    since_walk: Changed,
}

/// A change set no build of this daemon has bounded yet
impl Default for TreeDirt {
    fn default() -> Self {
        Self {
            since_publish: Changed::Unbounded,
            since_walk: Changed::default(),
        }
    }
}

/// Change sets the daemon keeps for every tree it watches
#[derive(Default)]
pub struct Dirt {
    trees: HashMap<PathBuf, TreeDirt>,
}

impl Dirt {
    /// Fold one drain of the watcher into both of a tree's change sets
    pub fn record(&mut self, state_root: &Path, changes: &TreeChanges) {
        let tree = self.tree(state_root);
        tree.since_publish.absorb(changes);
        tree.since_walk.absorb(changes);
    }

    /// Start counting changes against the walk a build is about to run
    pub fn begin_build(&mut self, state_root: &Path) {
        self.tree(state_root).since_walk = Changed::default();
    }

    /// Carry the changes a build raced into the new generation's change set
    pub fn commit_build(&mut self, state_root: &Path) {
        let tree = self.tree(state_root);
        tree.since_publish = std::mem::take(&mut tree.since_walk);
    }

    /// True when nothing changed since the running build's walk began
    pub fn build_was_uncontended(&self, state_root: &Path) -> bool {
        self.trees
            .get(state_root)
            .is_none_or(|tree| tree.since_walk.is_empty())
    }

    pub fn forget(&mut self, state_root: &Path) {
        self.trees.remove(state_root);
    }

    /// Write the change set a query reads instead of waiting for a rebuild
    pub fn publish(
        &mut self,
        state_root: &Path,
        index_root: &Path,
        drained_at: SystemTime,
    ) -> std::io::Result<()> {
        let body = self.rendered(state_root, index_root, drained_at);
        let runtime = state_root.join(RUNTIME_DIR_NAME);
        fs::create_dir_all(&runtime)?;
        let staging = runtime.join(format!("{DIRTY_FILE_NAME}.{}.staging", std::process::id()));
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&staging)?;
        file.write_all(body.as_bytes())?;
        drop(file);
        fs::rename(staging, runtime.join(DIRTY_FILE_NAME))
    }

    fn rendered(&mut self, state_root: &Path, index_root: &Path, drained_at: SystemTime) -> String {
        let mut body = format!("drained={}\n", unix_nanos(drained_at));
        match &self.tree(state_root).since_publish {
            Changed::Unbounded => body.push_str(UNBOUNDED_MARKER),
            Changed::Paths(paths) => render_paths(&mut body, paths, index_root),
        }
        body.push('\n');
        body
    }

    fn tree(&mut self, state_root: &Path) -> &mut TreeDirt {
        self.trees.entry(state_root.to_path_buf()).or_default()
    }
}

/// Name each changed path relative to the corpus root, one hex line each
fn render_paths(body: &mut String, paths: &BTreeSet<PathBuf>, index_root: &Path) {
    for path in paths {
        let Ok(relative) = path.strip_prefix(index_root) else {
            continue;
        };
        body.push_str("path=");
        body.push_str(&hex_encode(relative.as_os_str().as_encoded_bytes()));
        body.push('\n');
    }
}

fn unix_nanos(time: SystemTime) -> u128 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{DIRTY_CEILING, DIRTY_FILE_NAME, Dirt, UNBOUNDED_MARKER};
    use crate::watch::{DirtyEvents, TreeChanges};
    use std::{
        fs,
        path::{Path, PathBuf},
        time::SystemTime,
    };

    fn changes(paths: &[&str], coarse: bool) -> TreeChanges {
        let mut events = DirtyEvents::default();
        let state_root = Path::new("/repo/.eg");
        if paths.is_empty() {
            events.widen(&[state_root.to_path_buf()]);
        }
        for path in paths {
            events.record(state_root, PathBuf::from(path), coarse);
        }
        events
            .into_trees()
            .next()
            .map(|(_, changes)| changes)
            .expect("one tree")
    }

    fn published_body(dirt: &mut Dirt, state_root: &Path) -> String {
        dirt.publish(state_root, Path::new("/repo"), SystemTime::now())
            .expect("publish");
        fs::read_to_string(state_root.join("runtime").join(DIRTY_FILE_NAME)).expect("read")
    }

    fn scratch(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("eg-dirt-{name}-"))
            .tempdir()
            .expect("scratch dir")
    }

    #[test]
    fn a_change_racing_the_walk_survives_into_the_new_generation() {
        let guard = scratch("mid-walk");
        let state_root = guard.path();
        let mut dirt = Dirt::default();
        dirt.record(state_root, &changes(&["/repo/before.txt"], false));

        dirt.begin_build(state_root);
        dirt.record(state_root, &changes(&["/repo/during.txt"], false));
        assert!(!dirt.build_was_uncontended(state_root));
        dirt.commit_build(state_root);

        let body = published_body(&mut dirt, state_root);
        assert!(body.contains(&super::hex_encode(b"during.txt")));
        assert!(!body.contains(&super::hex_encode(b"before.txt")));
    }

    #[test]
    fn an_uncontended_build_publishes_an_empty_change_set() {
        let guard = scratch("quiet");
        let state_root = guard.path();
        let mut dirt = Dirt::default();
        dirt.record(state_root, &changes(&["/repo/before.txt"], false));

        dirt.begin_build(state_root);
        assert!(dirt.build_was_uncontended(state_root));
        dirt.commit_build(state_root);

        let body = published_body(&mut dirt, state_root);
        assert!(!body.contains("path="));
        assert!(body.starts_with("drained="));
    }

    #[test]
    fn a_directory_event_makes_the_change_set_unbounded() {
        let guard = scratch("coarse");
        let state_root = guard.path();
        let mut dirt = Dirt::default();

        dirt.record(state_root, &changes(&["/repo/moved"], true));

        assert!(published_body(&mut dirt, state_root).contains(UNBOUNDED_MARKER));
    }

    #[test]
    fn too_many_changed_paths_become_unbounded() {
        let guard = scratch("ceiling");
        let state_root = guard.path();
        let mut dirt = Dirt::default();
        let paths: Vec<String> = (0..=DIRTY_CEILING)
            .map(|index| format!("/repo/file{index}.txt"))
            .collect();
        let borrowed: Vec<&str> = paths.iter().map(String::as_str).collect();

        dirt.record(state_root, &changes(&borrowed, false));

        assert!(published_body(&mut dirt, state_root).contains(UNBOUNDED_MARKER));
    }

    #[test]
    fn paths_outside_the_corpus_root_are_not_published() {
        let guard = scratch("outside");
        let state_root = guard.path();
        let mut dirt = Dirt::default();

        dirt.record(state_root, &changes(&["/elsewhere/file.txt"], false));

        assert!(!published_body(&mut dirt, state_root).contains("path="));
    }
}
