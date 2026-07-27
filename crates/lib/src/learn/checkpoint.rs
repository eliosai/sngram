use std::sync::atomic::Ordering;

use sngram_types::LearnError;

use super::{BigramCounter, LearnSettings};

impl BigramCounter {
    /// All pair counts as little-endian `u64` bytes for checkpointing
    #[must_use]
    pub fn snapshot(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(LearnSettings::SNAPSHOT_BYTES);
        for count in self.counts.iter().map(|c| c.load(Ordering::Relaxed)) {
            out.extend_from_slice(&count.to_le_bytes());
        }
        out
    }

    /// Restore a checkpoint into a fresh counter
    ///
    /// # Errors
    ///
    /// Returns [`LearnError`] for an invalid snapshot or non-fresh counter
    pub fn restore(
        &self,
        snapshot: &[u8],
        pairs: u64,
        bytes: u64,
        files: u64,
    ) -> Result<(), LearnError> {
        if snapshot.len() != LearnSettings::SNAPSHOT_BYTES {
            return Err(LearnError::InvalidSnapshotLen {
                expected: LearnSettings::SNAPSHOT_BYTES,
                actual: snapshot.len(),
            });
        }
        if !self.is_fresh() {
            return Err(LearnError::NotFresh);
        }
        self.restore_counts(snapshot);
        self.pairs_processed.store(pairs, Ordering::Relaxed);
        self.bytes_processed.store(bytes, Ordering::Relaxed);
        self.files_processed.store(files, Ordering::Relaxed);
        Ok(())
    }

    fn restore_counts(&self, snapshot: &[u8]) {
        for (idx, chunk) in snapshot.chunks_exact(8).enumerate() {
            let mut bytes = [0; 8];
            bytes.copy_from_slice(chunk);
            let n = u64::from_le_bytes(bytes);
            if n > 0 {
                self.add_pair_by_index(idx, n);
            }
        }
    }

    fn is_fresh(&self) -> bool {
        self.pairs_processed() == 0
            && self.bytes_processed() == 0
            && self.files_processed() == 0
            && self.counts.iter().all(|c| c.load(Ordering::Relaxed) == 0)
    }
}
