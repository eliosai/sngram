//! Upstream source metadata for copied ripgrep code.

/// Git repository that supplied the copied binary facade.
pub(crate) const RIPGREP_REPOSITORY: &str = "https://github.com/BurntSushi/ripgrep";

/// Commit copied into this crate.
pub(crate) const RIPGREP_COMMIT: &str = "48b0c795f4feb37343b2832d991c5c6a3900c08a";

/// ripgrep package version at the copied commit.
pub(crate) const RIPGREP_VERSION: &str = "15.1.0";
