//! CLI configuration for indexed search.

use std::path::PathBuf;

/// Whether indexed search is enabled for this invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexUse {
    /// Do not use an index.
    Disabled,
    /// Use the daemon-owned sparse n-gram index.
    Enabled,
}

impl Default for IndexUse {
    fn default() -> Self {
        Self::Enabled
    }
}

impl IndexUse {
    /// Return true when the copied ripgrep path should run directly.
    const fn is_disabled(self) -> bool {
        matches!(self, Self::Disabled)
    }
}

/// Index backend selected by the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexBackend {
    /// eg's compact mmap-backed postings index.
    Postings,
    /// Tantivy on disk, using its mmap-backed directory.
    Tantivy,
}

impl Default for IndexBackend {
    fn default() -> IndexBackend {
        IndexBackend::Postings
    }
}

/// Parsed eg index settings.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IndexConfig {
    /// Whether indexed search is enabled.
    use_index: IndexUse,
    /// Selected index backend.
    backend: IndexBackend,
    /// Explicit index-state directory overriding the default `.eg` location.
    dir: Option<PathBuf>,
    /// Emit indexed-search benchmark data instead of match output.
    bench: bool,
}

impl IndexConfig {
    /// Set the index mode from its CLI value.
    pub fn set_mode(&mut self, mode: &str) -> anyhow::Result<()> {
        self.use_index = match mode {
            "auto" => IndexUse::Enabled,
            other => anyhow::bail!("unrecognized index mode '{other}', expected auto"),
        };
        Ok(())
    }

    /// Set the index backend from its CLI value.
    pub fn set_backend(&mut self, backend: &str) -> anyhow::Result<()> {
        self.backend = match backend {
            "postings" => IndexBackend::Postings,
            "tantivy" => IndexBackend::Tantivy,
            other => {
                anyhow::bail!("unrecognized index backend '{other}', expected postings or tantivy")
            },
        };
        self.enable_if_disabled();
        Ok(())
    }

    /// Store an explicit index-state directory and enable the index if needed.
    pub fn set_dir(&mut self, dir: PathBuf) {
        self.dir = Some(dir);
        self.enable_if_disabled();
    }

    /// Disable indexed search.
    pub fn disable(&mut self) {
        self.use_index = IndexUse::Disabled;
    }

    /// Enable structured benchmark output for indexed search.
    pub fn enable_bench(&mut self) {
        self.bench = true;
        self.enable_if_disabled();
    }

    /// Return true when the copied ripgrep path should run directly.
    pub fn is_no_index(&self) -> bool {
        self.use_index.is_disabled()
    }

    /// Return the explicit index-state directory, if configured.
    pub fn dir(&self) -> Option<&PathBuf> {
        self.dir.as_ref()
    }

    pub const fn bench(&self) -> bool {
        self.bench
    }

    pub const fn mode_name(&self) -> &'static str {
        match self.use_index {
            IndexUse::Disabled => "no-index",
            IndexUse::Enabled => "auto",
        }
    }

    pub const fn backend_name(&self) -> &'static str {
        match self.backend {
            IndexBackend::Postings => "postings",
            IndexBackend::Tantivy => "tantivy",
        }
    }

    pub const fn backend(&self) -> IndexBackend {
        self.backend
    }

    fn enable_if_disabled(&mut self) {
        if self.use_index.is_disabled() {
            self.use_index = IndexUse::Enabled;
        }
    }
}
