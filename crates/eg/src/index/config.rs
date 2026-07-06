//! CLI configuration for indexed search.

use std::path::PathBuf;

/// Index mode selected by the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexMode {
    /// Do not use an index.
    NoIndex,
    /// Use an existing compatible index.
    Auto,
    /// Rebuild the index before searching.
    Rebuild,
    /// Require the index and never fall back to a scan.
    Require,
    /// Check the index for structural faults and report, without searching.
    Verify,
    /// Verify the index and rebuild it when a fault is found.
    Repair,
}

impl Default for IndexMode {
    fn default() -> IndexMode {
        IndexMode::Auto
    }
}

impl IndexMode {
    /// Return true when the copied ripgrep path should run directly.
    fn is_no_index(self) -> bool {
        matches!(self, IndexMode::NoIndex)
    }

    /// Return true for a maintenance mode that inspects instead of searches.
    pub const fn is_maintenance(self) -> bool {
        matches!(self, IndexMode::Verify | IndexMode::Repair)
    }
}

/// Freshness policy deciding when a file counts as changed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IndexFreshness {
    /// Compare modification time, change time, and length.
    #[default]
    Stat,
    /// Compare a fast content hash over the head and tail windows and length.
    Hash,
}

impl IndexFreshness {
    /// Return true when freshness uses a content hash instead of stat fields.
    pub const fn is_hash(self) -> bool {
        matches!(self, IndexFreshness::Hash)
    }
}

/// Index backend selected by the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexBackend {
    /// eg's compact mmap-backed postings index.
    Postings,
    /// Tantivy on disk, using its mmap-backed directory.
    Tantivy,
    /// Tantivy in memory. This is for benchmark isolation.
    TantivyRam,
}

impl Default for IndexBackend {
    fn default() -> IndexBackend {
        IndexBackend::Postings
    }
}

/// Parsed eg index settings.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IndexConfig {
    /// Selected index mode.
    mode: IndexMode,
    /// Selected index backend.
    backend: IndexBackend,
    /// Selected freshness policy.
    freshness: IndexFreshness,
    /// Explicit index-state directory overriding the default `.eg` location.
    dir: Option<PathBuf>,
    /// Emit structured indexed-search benchmark data instead of match output.
    bench: bool,
    /// Run the embedded regex suite through indexed and unindexed search.
    bench_suite: bool,
}

impl IndexConfig {
    /// Set the index mode from its CLI value.
    pub fn set_mode(&mut self, mode: &str) -> anyhow::Result<()> {
        self.mode = match mode {
            "auto" => IndexMode::Auto,
            "rebuild" => IndexMode::Rebuild,
            "require" => IndexMode::Require,
            "verify" => IndexMode::Verify,
            "repair" => IndexMode::Repair,
            other => anyhow::bail!(
                "unrecognized index mode '{other}', expected auto, rebuild, require, verify or repair"
            ),
        };
        Ok(())
    }

    /// Set the index backend from its CLI value.
    pub fn set_backend(&mut self, backend: &str) -> anyhow::Result<()> {
        self.backend = match backend {
            "postings" => IndexBackend::Postings,
            "tantivy" => IndexBackend::Tantivy,
            "tantivy-ram" => IndexBackend::TantivyRam,
            other => anyhow::bail!(
                "unrecognized index backend '{other}', expected postings, tantivy or tantivy-ram"
            ),
        };
        self.enable_if_disabled();
        Ok(())
    }

    /// Set the freshness policy from its CLI value.
    pub fn set_freshness(&mut self, freshness: &str) -> anyhow::Result<()> {
        self.freshness = match freshness {
            "stat" => IndexFreshness::Stat,
            "hash" => IndexFreshness::Hash,
            other => anyhow::bail!("unrecognized index freshness '{other}', expected stat or hash"),
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
        self.mode = IndexMode::NoIndex;
    }

    /// Enable structured benchmark output for indexed search.
    pub fn enable_bench(&mut self) {
        self.bench = true;
        self.enable_if_disabled();
    }

    /// Enable the embedded indexed-vs-unindexed benchmark suite.
    pub fn enable_bench_suite(&mut self) {
        self.bench_suite = true;
        self.enable_if_disabled();
    }

    /// Return true when the copied ripgrep path should run directly.
    pub fn is_no_index(&self) -> bool {
        self.mode.is_no_index()
    }

    /// Return true when the selected mode inspects or repairs without search.
    pub fn is_maintenance(&self) -> bool {
        self.mode.is_maintenance()
    }

    /// Return the explicit index-state directory, if configured.
    pub fn dir(&self) -> Option<&PathBuf> {
        self.dir.as_ref()
    }

    pub const fn bench(&self) -> bool {
        self.bench
    }

    pub const fn bench_suite(&self) -> bool {
        self.bench_suite
    }

    pub const fn mode_name(&self) -> &'static str {
        match self.mode {
            IndexMode::NoIndex => "no-index",
            IndexMode::Auto => "auto",
            IndexMode::Rebuild => "rebuild",
            IndexMode::Require => "require",
            IndexMode::Verify => "verify",
            IndexMode::Repair => "repair",
        }
    }

    pub const fn backend_name(&self) -> &'static str {
        match self.backend {
            IndexBackend::Postings => "postings",
            IndexBackend::Tantivy => "tantivy",
            IndexBackend::TantivyRam => "tantivy-ram",
        }
    }

    pub const fn mode(&self) -> IndexMode {
        self.mode
    }

    pub const fn backend(&self) -> IndexBackend {
        self.backend
    }

    pub const fn freshness(&self) -> IndexFreshness {
        self.freshness
    }

    fn enable_if_disabled(&mut self) {
        if self.mode.is_no_index() {
            self.mode = IndexMode::Auto;
        }
    }
}
