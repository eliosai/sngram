//! Byte-pair counting that trains a weight table, one value at a time so no pair straddles two inputs

use std::sync::atomic::{AtomicU64, Ordering};

mod batch;
mod checkpoint;
mod mint;
mod settings;

use batch::BatchCounts;
use settings::LearnSettings;

/// Why a checkpoint does not restore
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum LearnError {
    /// The snapshot is not one little-endian `u64` per pair
    #[error("snapshot must be {expected} bytes, got {actual}")]
    InvalidSnapshotLen {
        /// The byte length a snapshot has
        expected: usize,
        /// The byte length this one has
        actual: usize,
    },
    /// The counter already holds counts
    #[error("cannot restore into a non-empty counter")]
    NotFresh,
}

/// A byte-pair frequency counter many threads write at once
pub struct BigramCounter {
    counts: Box<[AtomicU64; LearnSettings::PAIR_COUNT]>,
    pairs_processed: AtomicU64,
    bytes_processed: AtomicU64,
    files_processed: AtomicU64,
}

impl Default for BigramCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl BigramCounter {
    /// A counter with every count at zero
    #[must_use]
    pub fn new() -> Self {
        let counts: Box<[AtomicU64; LearnSettings::PAIR_COUNT]> = (0..LearnSettings::PAIR_COUNT)
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice()
            .try_into()
            .unwrap_or_else(|_| unreachable!("pair-count elements"));
        Self {
            counts,
            pairs_processed: AtomicU64::new(0),
            bytes_processed: AtomicU64::new(0),
            files_processed: AtomicU64::new(0),
        }
    }

    /// Fold a completed staging counter into this one
    pub fn merge(&self, other: &Self) {
        for (idx, count) in other.counts.iter().enumerate() {
            let n = count.load(Ordering::Relaxed);
            if n > 0 {
                self.counts[idx].fetch_add(n, Ordering::Relaxed);
            }
        }
        self.pairs_processed
            .fetch_add(other.pairs_processed(), Ordering::Relaxed);
        self.bytes_processed
            .fetch_add(other.bytes_processed(), Ordering::Relaxed);
        self.files_processed
            .fetch_add(other.files_processed(), Ordering::Relaxed);
    }

    /// Count the byte pairs of one value
    pub fn process(&self, content: &[u8]) {
        self.process_batch(core::iter::once(content));
    }

    /// Count many values and merge them once, with no pair counted across two values
    pub fn process_batch<'a, I>(&self, values: I) -> u64
    where
        I: IntoIterator<Item = &'a [u8]>,
    {
        let mut batch = BatchCounts::new();
        for value in values {
            batch.count_buffer(value);
        }
        let bytes = batch.bytes();
        self.merge_batch(&batch);
        bytes
    }

    /// Record completed files or shards
    pub fn add_files(&self, n: u64) {
        self.files_processed.fetch_add(n, Ordering::Relaxed);
    }

    /// Byte pairs counted so far
    #[must_use]
    pub fn pairs_processed(&self) -> u64 {
        self.pairs_processed.load(Ordering::Relaxed)
    }

    /// Bytes counted so far
    #[must_use]
    pub fn bytes_processed(&self) -> u64 {
        self.bytes_processed.load(Ordering::Relaxed)
    }

    /// Files or shards completed so far
    #[must_use]
    pub fn files_processed(&self) -> u64 {
        self.files_processed.load(Ordering::Relaxed)
    }

    /// The count of one byte pair
    #[must_use]
    pub fn count(&self, c1: u8, c2: u8) -> u64 {
        self.counts[LearnSettings::pair_index(c1, c2)].load(Ordering::Relaxed)
    }

    fn merge_batch(&self, batch: &BatchCounts) {
        for (idx, &n) in batch.pair_counts().iter().enumerate() {
            if n > 0 {
                self.counts[idx].fetch_add(u64::from(n), Ordering::Relaxed);
            }
        }
        self.pairs_processed
            .fetch_add(batch.pairs_counted(), Ordering::Relaxed);
        self.bytes_processed
            .fetch_add(batch.bytes(), Ordering::Relaxed);
    }

    fn add_pair_by_index(&self, idx: usize, n: u64) {
        if idx < LearnSettings::PAIR_COUNT {
            self.counts[idx].fetch_add(n, Ordering::Relaxed);
        }
    }

    #[cfg(test)]
    fn add_pair(&self, c1: u8, c2: u8, n: u64) {
        self.add_pair_by_index(LearnSettings::pair_index(c1, c2), n);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::LearnError;
    use crate::WeightTable;

    use super::*;

    #[test]
    fn empty_counter_produces_valid_table() {
        let counter = BigramCounter::new();
        let table = WeightTable::from_bytes(&counter.to_table_bytes()).unwrap();
        assert_eq!(table.weight(0, 0), u32::MAX);
    }

    #[test]
    fn counts_byte_pairs_and_tracks_progress() {
        let counter = BigramCounter::new();
        counter.process(b"aab");
        counter.process(b"x");

        assert_eq!(counter.count(b'a', b'a'), 1);
        assert_eq!(counter.count(b'a', b'b'), 1);
        assert_eq!(counter.count(b'b', b'a'), 0);
        assert_eq!(counter.pairs_processed(), 2);
        assert_eq!(counter.bytes_processed(), 4);
    }

    #[test]
    fn files_are_tracked_separately() {
        let counter = BigramCounter::new();
        counter.add_files(2);
        assert_eq!(counter.files_processed(), 2);
        assert_eq!(counter.pairs_processed(), 0);
    }

    #[test]
    fn merge_matches_direct_reference() {
        let counter = BigramCounter::new();
        let staging = BigramCounter::new();
        let data = b"the quick brown fox jumps over the lazy dog";
        staging.process(data);
        counter.merge(&staging);

        let (reference, total) = reference_counts(&[data]);
        assert_eq!(counter.pairs_processed(), total);
        for (&(a, b), &n) in &reference {
            assert_eq!(counter.count(a, b), n, "mismatch at ({a},{b})");
        }
    }

    #[test]
    fn snapshot_restore_round_trips_counts_and_totals() {
        let original = BigramCounter::new();
        original.process(b"ababa");
        original.add_files(3);

        let restored = BigramCounter::new();
        restored
            .restore(
                &original.snapshot(),
                original.pairs_processed(),
                original.bytes_processed(),
                original.files_processed(),
            )
            .unwrap();

        assert_eq!(restored.snapshot(), original.snapshot());
        assert_eq!(restored.pairs_processed(), original.pairs_processed());
        assert_eq!(restored.bytes_processed(), original.bytes_processed());
        assert_eq!(restored.files_processed(), original.files_processed());
        assert_eq!(restored.to_table_bytes(), original.to_table_bytes());
    }

    #[test]
    fn restore_rejects_bad_snapshot_len_and_non_fresh_counter() {
        let counter = BigramCounter::new();
        assert!(matches!(
            counter.restore(&[0; 7], 0, 0, 0),
            Err(LearnError::InvalidSnapshotLen { .. })
        ));

        counter.process(b"ab");
        let snapshot = BigramCounter::new().snapshot();
        assert_eq!(
            counter.restore(&snapshot, 0, 0, 0),
            Err(LearnError::NotFresh)
        );
    }

    #[test]
    fn pair_index_edges_restore_and_serialize() {
        let counter = BigramCounter::new();
        for (left, right, count) in [
            (0u8, 0u8, 3u64),
            (0, u8::MAX, 7),
            (u8::MAX, 0, 11),
            (u8::MAX, u8::MAX, 5),
        ] {
            counter.add_pair(left, right, count);
        }
        counter.pairs_processed.store(26, Ordering::Relaxed);

        let table = WeightTable::from_bytes(&counter.to_table_bytes()).unwrap();
        assert_eq!(table.weight(0, 0), 26 / 3);
        assert_eq!(table.weight(0, u8::MAX), 26 / 7);
        assert_eq!(table.weight(u8::MAX, 0), 26 / 11);
        assert_eq!(table.weight(u8::MAX, u8::MAX), 26 / 5);
    }

    #[test]
    fn learned_table_matches_independent_reference() {
        let corpus: &[&[u8]] = &[
            b"fn main() { let x = 42; }",
            b"the quick brown fox jumps over the lazy dog",
            b"SELECT * FROM users WHERE id = 1;",
            b"\x00\x01\x02\xc8\xff\xfe\x00\x01",
        ];
        let (counts, total) = reference_counts(corpus);
        let counter = BigramCounter::new();
        for row in corpus {
            counter.process(row);
        }
        let table = WeightTable::from_bytes(&counter.to_table_bytes()).unwrap();

        assert_eq!(counter.pairs_processed(), total);
        assert_table_matches_reference(&table, &counts, total);
    }

    #[test]
    fn concurrent_merge_is_deterministic() {
        let counter = Arc::new(BigramCounter::new());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let counter = counter.clone();
                std::thread::spawn(move || {
                    let staging = repeated_staging_counter(b"ab", 1000);
                    counter.merge(&staging);
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(counter.count(b'a', b'b'), 8000);
    }

    fn repeated_staging_counter(value: &[u8], repeats: usize) -> BigramCounter {
        let counter = BigramCounter::new();
        counter.process_batch(core::iter::repeat_n(value, repeats));
        counter
    }

    fn reference_counts(corpus: &[&[u8]]) -> (HashMap<(u8, u8), u64>, u64) {
        let mut counts = HashMap::new();
        let mut total: u64 = 0;
        for row in corpus {
            for pair in row.windows(2) {
                *counts.entry((pair[0], pair[1])).or_default() += 1;
                total += 1;
            }
        }
        (counts, total)
    }

    fn assert_table_matches_reference(
        table: &WeightTable,
        counts: &HashMap<(u8, u8), u64>,
        total: u64,
    ) {
        for c1 in 0u8..=255 {
            for c2 in 0u8..=255 {
                let count = counts.get(&(c1, c2)).copied().unwrap_or(0);
                assert_eq!(
                    table.weight(c1, c2),
                    expected_weight(total, count),
                    "weight ({c1},{c2})"
                );
            }
        }
    }

    #[allow(clippy::cast_possible_truncation, reason = "min() clamps to u32 range")]
    fn expected_weight(total: u64, count: u64) -> u32 {
        total
            .checked_div(count)
            .map_or(u32::MAX, |w| w.min(u64::from(u32::MAX)) as u32)
    }
}
