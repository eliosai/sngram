//! Resolves where the sparse n-gram index state lives.
//!
//! The corpus root is where relative paths and Git freshness are anchored; the
//! state root is where the `.eg` directory (index files, manifest, lock) is
//! written. They differ when the corpus is read-only and the index falls back
//! to the XDG cache, or when `--index-dir` overrides the location.

use std::{
    env, fs,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::Context;

use crate::flags::HiArgs;

const STATE_DIR_NAME: &str = ".eg";
const INDEX_DIR_NAME: &str = "index";
const GITIGNORE_NAME: &str = ".gitignore";
const CACHE_APP_DIR: &str = "eg";

/// Resolved index locations for one invocation.
pub struct IndexLocation {
    /// Corpus root anchoring relative paths and Git detection.
    pub corpus_root: PathBuf,
    /// Directory holding the index files, manifest, and lock.
    pub state_root: PathBuf,
}

impl IndexLocation {
    /// Directory holding the backend index files.
    pub fn index_dir(&self) -> PathBuf {
        self.state_root.join(INDEX_DIR_NAME)
    }
}

/// Resolve the corpus and state roots, creating the state directory.
///
/// The corpus root stays as given so manifest paths and Git detection match the
/// walk; only the XDG cache name canonicalizes it so placement is cwd-stable.
pub fn resolve(args: &HiArgs, corpus_root: &Path) -> anyhow::Result<IndexLocation> {
    let corpus_root = corpus_root.to_path_buf();
    let state_root = args.index().dir().map_or_else(
        || default_state_root(&corpus_root),
        |dir| super::absolute_path(args.cwd(), dir),
    );
    ensure_state_root(&state_root)?;
    Ok(IndexLocation {
        corpus_root,
        state_root,
    })
}

/// Choose the local `.eg` directory when writable, else the XDG cache.
fn default_state_root(corpus_root: &Path) -> PathBuf {
    let local = local_state_root(corpus_root);
    if local_is_usable(&local) {
        return local;
    }
    log::debug!(
        "eg index: corpus {} is not writable; using XDG cache",
        corpus_root.display()
    );
    cache_state_root(corpus_root)
}

/// Local state directory for an index root, without creating it.
pub fn local_state_root(corpus_root: &Path) -> PathBuf {
    corpus_root.join(STATE_DIR_NAME)
}

/// Return true when the local state directory exists or can be created.
fn local_is_usable(local: &Path) -> bool {
    fs::create_dir_all(local).is_ok() && write_probe(local).is_ok()
}

/// Build the per-corpus XDG cache directory, keyed by the canonical corpus path.
fn cache_state_root(corpus_root: &Path) -> PathBuf {
    let canonical = fs::canonicalize(corpus_root).unwrap_or_else(|_| corpus_root.to_path_buf());
    cache_home().join(CACHE_APP_DIR).join(hash_hex(&canonical))
}

/// Locate the XDG cache home, falling back to `$HOME/.cache` then a temp dir.
fn cache_home() -> PathBuf {
    if let Some(dir) = non_empty_var("XDG_CACHE_HOME") {
        return PathBuf::from(dir);
    }
    if let Some(home) = non_empty_var("HOME") {
        return PathBuf::from(home).join(".cache");
    }
    env::temp_dir().join("eg-cache")
}

/// Read an environment variable, treating empty values as unset.
fn non_empty_var(name: &str) -> Option<std::ffi::OsString> {
    let value = env::var_os(name)?;
    if value.is_empty() { None } else { Some(value) }
}

/// Create the state root and write a self-ignoring `.gitignore`.
fn ensure_state_root(state_root: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(state_root).with_context(|| {
        format!(
            "failed to create index state directory {}",
            state_root.display()
        )
    })?;
    write_probe(state_root).with_context(|| {
        format!(
            "index state directory {} is not writable",
            state_root.display()
        )
    })?;
    write_gitignore(state_root);
    Ok(())
}

/// Prove the state directory is writable without relying on mode bits.
fn write_probe(state_root: &Path) -> std::io::Result<()> {
    let path = state_root.join(format!(".eg-write-test-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;
    file.write_all(b"ok")?;
    drop(file);
    fs::remove_file(path)
}

/// Write `*` into the state directory's `.gitignore` so repos stay clean.
fn write_gitignore(state_root: &Path) {
    let path = state_root.join(GITIGNORE_NAME);
    if path.exists() {
        return;
    }
    if let Ok(mut file) = fs::File::create(&path) {
        let _ = file.write_all(b"*\n");
    }
}

/// FNV-1a hex digest of a path, used to name the per-corpus cache directory.
fn hash_hex(path: &Path) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in path.as_os_str().to_string_lossy().bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::{cache_state_root, hash_hex};
    use std::path::Path;

    #[test]
    fn hash_is_stable_and_path_specific() {
        let a = hash_hex(Path::new("/home/user/proj"));
        let b = hash_hex(Path::new("/home/user/proj"));
        let c = hash_hex(Path::new("/home/user/other"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(16, a.len());
    }

    #[test]
    fn cache_state_root_is_per_corpus() {
        let one = cache_state_root(Path::new("/a/b"));
        let two = cache_state_root(Path::new("/a/c"));
        assert!(one.ends_with(hash_hex(Path::new("/a/b"))));
        assert_ne!(one, two);
    }
}
