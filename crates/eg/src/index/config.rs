//! CLI configuration for indexed search.

/// Index mode selected by the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IndexMode {
    /// Do not use an index.
    NoIndex,
    /// Use an existing compatible index.
    Auto,
    /// Rebuild the index before searching.
    Rebuild,
}

impl Default for IndexMode {
    fn default() -> IndexMode {
        IndexMode::NoIndex
    }
}

impl IndexMode {
    /// Return true when the copied ripgrep path should run directly.
    pub(crate) fn is_no_index(self) -> bool {
        matches!(self, IndexMode::NoIndex)
    }
}

/// Index backend selected by the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IndexBackend {
    /// Tantivy on disk, using its mmap-backed directory.
    Tantivy,
    /// Tantivy in memory. This is for benchmark isolation.
    TantivyRam,
}

impl Default for IndexBackend {
    fn default() -> IndexBackend {
        IndexBackend::Tantivy
    }
}

/// Parsed eg index settings.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct IndexConfig {
    /// Selected index mode.
    pub(crate) mode: IndexMode,
    /// Selected index backend.
    pub(crate) backend: IndexBackend,
}
