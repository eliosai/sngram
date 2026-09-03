//! Error types for table loading.

/// Errors when loading a weight table from bytes.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TableError {
    /// Binary data is not 262,160 bytes.
    #[error("invalid table size: expected 262160, got {0}")]
    InvalidSize(usize),

    /// Missing "SPNG" magic header.
    #[error("invalid magic: expected SPNG header")]
    InvalidMagic,

    /// CRC32 checksum mismatch.
    #[error("checksum mismatch: expected {expected:#010x}, got {actual:#010x}")]
    Checksum {
        /// Checksum stored in header.
        expected: u32,
        /// Checksum computed from data.
        actual: u32,
    },

    /// Data ends before a complete header or field.
    #[error("truncated table data: {0} bytes")]
    Truncated(usize),

    /// Header carries a version this build does not read.
    #[error("unsupported table version: {0}")]
    InvalidVersion(u32),

    /// Provenance block is malformed or not UTF-8.
    #[error("invalid provenance block")]
    InvalidProvenance,
}
