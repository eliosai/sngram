//! Which directories of a corpus tree the daemon watches.

use std::{
    fs,
    path::{Path, PathBuf},
};

/// Subdirectories of `root` that belong to the corpus rather than its state
pub fn child_dirs(root: &Path, state_root: &Path) -> anyhow::Result<Vec<PathBuf>> {
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

/// True for index state, whose own writes must never mark a tree dirty
pub fn is_state_path(path: &Path, state_root: &Path) -> bool {
    path == state_root
        || path.starts_with(state_root)
        || path
            .components()
            .any(|component| component.as_os_str() == ".eg")
}

#[cfg(test)]
mod tests {
    use super::{child_dirs, is_state_path};
    use std::{fs, path::Path};

    #[test]
    fn state_paths_are_recognized_anywhere_in_the_tree() {
        let state = Path::new("/repo/.eg");

        assert!(is_state_path(state, state));
        assert!(is_state_path(Path::new("/repo/.eg/index"), state));
        assert!(is_state_path(Path::new("/repo/src/.eg"), state));
        assert!(!is_state_path(Path::new("/repo/src"), state));
    }

    #[test]
    fn child_dirs_skip_files_and_state() {
        let guard = tempfile::tempdir().expect("scratch dir");
        let root = guard.path();
        let state = root.join(".eg");
        fs::create_dir_all(&state).expect("state");
        fs::create_dir_all(root.join("src")).expect("src");
        fs::write(root.join("README"), "text").expect("file");

        let dirs = child_dirs(root, &state).expect("child dirs");

        assert_eq!(vec![root.join("src")], dirs);
    }

    #[test]
    fn an_unreadable_root_yields_no_children() {
        let guard = tempfile::tempdir().expect("scratch dir");
        let missing = guard.path().join("gone");

        assert!(
            child_dirs(&missing, &missing.join(".eg"))
                .expect("must not error")
                .is_empty()
        );
    }
}
