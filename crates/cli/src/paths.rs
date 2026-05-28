use std::path::PathBuf;

#[must_use]
pub fn data_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "sngram")
        .map_or_else(|| PathBuf::from(".sngram"), |p| p.data_dir().to_path_buf())
}

#[must_use]
pub fn default_mint_dir() -> PathBuf {
    data_dir().join("bins")
}

/// Create `dir` and all parents if missing.
///
/// # Errors
///
/// Returns an error if the directory cannot be created.
pub fn ensure_dir(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("creating {}: {e}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mints_under_data_dir() {
        assert!(default_mint_dir().starts_with(data_dir()));
    }

    #[test]
    fn ensure_dir_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        ensure_dir(&nested).unwrap();
        ensure_dir(&nested).unwrap();
        assert!(nested.is_dir());
    }
}
