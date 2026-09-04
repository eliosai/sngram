//! Why an `SPNG` binary is not a table

/// Why an `SPNG` binary is not a table
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TableError {
    /// A v1 binary is not 262,160 bytes
    #[error("invalid table size: expected 262160, got {0}")]
    InvalidSize(usize),

    /// The binary does not start with `SPNG`
    #[error("invalid magic: expected SPNG header")]
    InvalidMagic,

    /// The body does not hash to the header's CRC32
    #[error("checksum mismatch: expected {expected:#010x}, got {actual:#010x}")]
    Checksum {
        /// The checksum the header stores
        expected: u32,
        /// The checksum the body hashes to
        actual: u32,
    },

    /// The binary ends before a complete header or field
    #[error("truncated table data: {0} bytes")]
    Truncated(usize),

    /// The header names a version this build does not read
    #[error("unsupported table version: {0}")]
    InvalidVersion(u32),

    /// The provenance block is malformed or not UTF-8
    #[error("invalid provenance block")]
    InvalidProvenance,
}
