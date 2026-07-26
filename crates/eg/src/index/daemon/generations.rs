//! What lives under one index directory, and which of it is dead.

use std::{
    fs::{self, OpenOptions},
    path::Path,
};

use super::reclaim::Reclaimed;

/// Generation directory this binary publishes for the postings backend
pub const POSTINGS_GENERATION: &str = "postings-v9";
/// Generation directory this binary publishes for the tantivy backend
pub const TANTIVY_GENERATION: &str = "tantivy-v2";

const REBUILDING_SUFFIX: &str = ".rebuilding";
const OLD_SUFFIX: &str = ".old";
const LOCK_SUFFIX: &str = ".lock";
/// Files that mark a directory as an index generation rather than user data
const GENERATION_MARKERS: [&str; 4] =
    ["manifest.bin", "manifest.json", "postings.bin", "table.bin"];

/// Remove interrupted build staging and generations this binary retired.
pub fn sweep(index_dir: &Path, lease_live: bool) -> Reclaimed {
    let mut reclaimed = Reclaimed::default();
    let Ok(entries) = fs::read_dir(index_dir) else {
        return reclaimed;
    };
    let serving = has_current_generation(index_dir);
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        match disposition(&path, &name) {
            Disposition::Keep => {},
            Disposition::Leftover { base } => {
                if build_lock_is_free(index_dir, &base) {
                    reclaimed.take(&path);
                }
            },
            Disposition::Orphan => {
                if serving || !lease_live {
                    reclaimed.take(&path);
                }
            },
        }
    }
    reclaimed
}

/// True when a published generation this binary reads is present.
pub fn has_current_generation(index_dir: &Path) -> bool {
    [POSTINGS_GENERATION, TANTIVY_GENERATION]
        .iter()
        .any(|generation| {
            GENERATION_MARKERS
                .iter()
                .any(|marker| index_dir.join(generation).join(marker).exists())
        })
}

enum Disposition {
    Keep,
    /// staging or superseded copy left by an interrupted build
    Leftover {
        base: String,
    },
    /// generation directory this binary no longer publishes
    Orphan,
}

/// Decide what one entry under the index directory is.
fn disposition(path: &Path, name: &str) -> Disposition {
    if !path.is_dir() {
        return Disposition::Keep;
    }
    if let Some(base) = strip_build_suffix(name) {
        if is_known_generation(base) {
            return Disposition::Leftover {
                base: base.to_owned(),
            };
        }
        return orphan_if_generation(path);
    }
    if is_known_generation(name) {
        return Disposition::Keep;
    }
    orphan_if_generation(path)
}

/// Only directories carrying generation files are ours to reclaim.
fn orphan_if_generation(path: &Path) -> Disposition {
    if GENERATION_MARKERS
        .iter()
        .any(|marker| path.join(marker).exists())
    {
        Disposition::Orphan
    } else {
        Disposition::Keep
    }
}

fn strip_build_suffix(name: &str) -> Option<&str> {
    name.strip_suffix(REBUILDING_SUFFIX)
        .or_else(|| name.strip_suffix(OLD_SUFFIX))
}

fn is_known_generation(name: &str) -> bool {
    name == POSTINGS_GENERATION || name == TANTIVY_GENERATION
}

/// True when no build holds the generation's lock, so its staging is dead.
fn build_lock_is_free(index_dir: &Path, base: &str) -> bool {
    let path = index_dir.join(format!("{base}{LOCK_SUFFIX}"));
    let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
        return true;
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            true
        },
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{LOCK_SUFFIX, OLD_SUFFIX, POSTINGS_GENERATION, REBUILDING_SUFFIX, sweep};
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    fn scratch(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("eg-generations-{name}-"))
            .tempdir()
            .expect("scratch dir")
    }

    fn generation(index_dir: &Path, name: &str, bytes: usize) -> PathBuf {
        let dir = index_dir.join(name);
        fs::create_dir_all(&dir).expect("generation");
        fs::write(dir.join("manifest.bin"), vec![b'x'; bytes]).expect("manifest");
        dir
    }

    fn build_lock(index_dir: &Path) -> PathBuf {
        index_dir.join(format!("{POSTINGS_GENERATION}{LOCK_SUFFIX}"))
    }

    #[test]
    fn live_current_generation_is_never_reclaimed() {
        let root_guard = scratch("live");
        let index_dir = root_guard.path().join("index");
        let live = generation(&index_dir, POSTINGS_GENERATION, 16);

        let reclaimed = sweep(&index_dir, true);

        assert_eq!(0, reclaimed.paths);
        assert!(live.join("manifest.bin").exists());
    }

    #[test]
    fn retired_generation_is_reclaimed_beside_a_live_one() {
        let root_guard = scratch("retired");
        let index_dir = root_guard.path().join("index");
        let live = generation(&index_dir, POSTINGS_GENERATION, 16);
        let retired = generation(&index_dir, "postings-v8", 64);

        let reclaimed = sweep(&index_dir, true);

        assert_eq!(1, reclaimed.paths);
        assert_eq!(64, reclaimed.bytes);
        assert!(!retired.exists());
        assert!(live.exists());
    }

    #[test]
    fn interrupted_build_staging_is_reclaimed_when_the_lock_is_free() {
        let root_guard = scratch("staging");
        let index_dir = root_guard.path().join("index");
        let live = generation(&index_dir, POSTINGS_GENERATION, 16);
        let staging = generation(
            &index_dir,
            &format!("{POSTINGS_GENERATION}{REBUILDING_SUFFIX}"),
            32,
        );
        let superseded = generation(&index_dir, &format!("{POSTINGS_GENERATION}{OLD_SUFFIX}"), 8);
        fs::write(build_lock(&index_dir), "").expect("lock");

        let reclaimed = sweep(&index_dir, true);

        assert_eq!(2, reclaimed.paths);
        assert!(!staging.exists());
        assert!(!superseded.exists());
        assert!(live.exists());
    }

    #[test]
    fn a_held_build_lock_keeps_the_staging_directory() {
        let root_guard = scratch("locked");
        let index_dir = root_guard.path().join("index");
        generation(&index_dir, POSTINGS_GENERATION, 16);
        let staging = generation(
            &index_dir,
            &format!("{POSTINGS_GENERATION}{REBUILDING_SUFFIX}"),
            32,
        );
        let lock = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(build_lock(&index_dir))
            .expect("lock");
        lock.lock().expect("hold build lock");

        let reclaimed = sweep(&index_dir, true);

        assert_eq!(0, reclaimed.paths);
        assert!(staging.exists());
    }

    #[test]
    fn unknown_directories_without_generation_files_are_left_alone() {
        let root_guard = scratch("foreign");
        let index_dir = root_guard.path().join("index");
        generation(&index_dir, POSTINGS_GENERATION, 16);
        let foreign = index_dir.join("notes");
        fs::create_dir_all(&foreign).expect("foreign");
        fs::write(foreign.join("todo.txt"), "keep me").expect("note");

        let reclaimed = sweep(&index_dir, true);

        assert_eq!(0, reclaimed.paths);
        assert!(foreign.join("todo.txt").exists());
    }
}
