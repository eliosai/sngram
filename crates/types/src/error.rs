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
}
