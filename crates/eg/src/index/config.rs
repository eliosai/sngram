//! CLI configuration for indexed search.

use std::path::PathBuf;

/// Index mode selected by the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IndexMode {
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
    pub(crate) fn is_no_index(self) -> bool {
        matches!(self, IndexMode::NoIndex)
    }

    /// Return true for a maintenance mode that inspects instead of searches.
    pub(crate) const fn is_maintenance(self) -> bool {
        matches!(self, IndexMode::Verify | IndexMode::Repair)
    }
}

/// Freshness policy deciding when a file counts as changed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum IndexFreshness {
    /// Compare modification time, change time, and length.
    #[default]
    Stat,
    /// Compare a fast content hash over the head and tail windows and length.
    Hash,
}

impl IndexFreshness {
    /// Return true when freshness uses a content hash instead of stat fields.
    pub(crate) const fn is_hash(self) -> bool {
        matches!(self, IndexFreshness::Hash)
    }
}

/// Index backend selected by the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IndexBackend {
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
pub(crate) struct IndexConfig {
    /// Selected index mode.
    pub(crate) mode: IndexMode,
    /// Selected index backend.
    pub(crate) backend: IndexBackend,
    /// Selected freshness policy.
    pub(crate) freshness: IndexFreshness,
    /// Explicit index-state directory overriding the default `.eg` location.
    pub(crate) dir: Option<PathBuf>,
}
