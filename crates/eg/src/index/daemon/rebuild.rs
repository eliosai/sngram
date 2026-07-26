//! The child process that rebuilds one tree's index.

use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
};

use super::markers;

const EG_BINARY_NAME: &str = "eg";
const DAEMON_REFRESH_ENV: &str = "EG_INDEX_DAEMON_REFRESH";
const RUNTIME_ROOT_ENV: &str = "EG_INDEXD_RUNTIME_ROOT";

/// One rebuild of one tree, replaying the query that asked for the index
pub struct Rebuild<'a> {
    cwd: &'a Path,
    args: &'a [OsString],
    state_root: &'a Path,
    eg_binary: Option<&'a Path>,
}

impl<'a> Rebuild<'a> {
    pub const fn new(
        cwd: &'a Path,
        args: &'a [OsString],
        state_root: &'a Path,
        eg_binary: Option<&'a Path>,
    ) -> Self {
        Self {
            cwd,
            args,
            state_root,
            eg_binary,
        }
    }

    /// Start the rebuild, or `None` when no eg binary is around to run it
    pub fn spawn(&self, runtime_root: &Path) -> anyhow::Result<Option<Child>> {
        markers::clear_journal_clean(self.state_root);
        let Some(binary) = self.binary() else {
            return Ok(None);
        };
        let child = Command::new(binary)
            .args(self.args)
            .current_dir(self.cwd)
            .env(DAEMON_REFRESH_ENV, "1")
            .env(RUNTIME_ROOT_ENV, runtime_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Some(child))
    }

    /// True when the rebuild published; a failed one leaves no proof behind
    pub fn published(&self, status: ExitStatus) -> bool {
        if !status.success() {
            markers::clear_journal_clean(self.state_root);
        }
        status.success()
    }

    /// The eg binary the request named, or the one beside this daemon
    fn binary(&self) -> Option<PathBuf> {
        self.eg_binary
            .filter(|binary| binary.exists())
            .map(Path::to_path_buf)
            .or_else(sibling_eg_binary)
    }
}

fn sibling_eg_binary() -> Option<PathBuf> {
    let current = env::current_exe().ok()?;
    let dir = current.parent()?;
    let binary = dir.join(EG_BINARY_NAME);
    binary.exists().then_some(binary)
}

#[cfg(test)]
mod tests {
    use super::Rebuild;
    use std::{fs, path::Path};

    fn scratch(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("eg-rebuild-{name}-"))
            .tempdir()
            .expect("scratch dir")
    }

    #[test]
    fn a_named_eg_binary_is_only_used_once_it_exists() {
        let guard = scratch("named");
        let root = guard.path();
        let binary = root.join("eg");
        let rebuild = Rebuild::new(root, &[], root, Some(&binary));

        assert_ne!(Some(binary.as_path()), rebuild.binary().as_deref());

        fs::write(&binary, "eg").expect("binary");
        assert_eq!(Some(binary.as_path()), rebuild.binary().as_deref());
    }

    #[test]
    fn a_rebuild_with_no_binary_anywhere_starts_nothing() {
        let guard = scratch("missing");
        let root = guard.path();
        let rebuild = Rebuild::new(root, &[], root, Some(Path::new("/nonexistent/eg")));

        if rebuild.binary().is_none() {
            assert!(rebuild.spawn(root).expect("spawn").is_none());
        }
    }
}
