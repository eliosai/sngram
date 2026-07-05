//! Learning types.

/// Error restoring a training checkpoint.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LearnError {
    /// Snapshot byte length is not exactly one little-endian `u64` per pair.
    #[error("snapshot must be {expected} bytes, got {actual}")]
    InvalidSnapshotLen {
        /// Expected byte length.
        expected: usize,
        /// Actual byte length.
        actual: usize,
    },
    /// Restore was attempted into a counter that already contains data.
    #[error("cannot restore into a non-empty counter")]
    NotFresh,
}
