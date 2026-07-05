//! Bigram counting for weight-table learning.
//!
//! The public learning API has one primary type: [`BigramCounter`]. Feed text
//! values directly into it with [`BigramCounter::process`] or
//! [`BigramCounter::process_batch`], merge completed staging counters with
//! [`BigramCounter::merge`], and serialize a learned `SPNG` weight table with
//! [`BigramCounter::to_table_bytes`].
//!
//! Counting is per-value: no bigram may straddle two inputs, so the learned
//! table is a function of the data alone, not of batch geometry.

use std::sync::atomic::{AtomicU64, Ordering};

use sngram_types::WeightTable;

mod batch;
mod mint;
mod settings;

use batch::BatchCounts;
use mint::{Tuning, compute_weight, tune_weight};
use settings::LearnSettings;
use sngram_types::LearnError;

/// Shared byte-pair frequency counter, written concurrently by workers.
///
/// `BigramCounter` is the learning abstraction: count text, merge completed
/// staging counters, inspect progress, checkpoint, and serialize a learned
/// weight table.
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
    /// Fresh counter with all counts zero.
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

    /// Fold a completed staging counter into this counter.
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

    /// Count one value's bytes directly.
    pub fn process(&self, content: &[u8]) {
        self.process_batch(core::iter::once(content));
    }

    /// Count many independent values and merge them once.
    ///
    /// No byte pair is counted across two values.
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

    /// Record completed files or shards.
    pub fn add_files(&self, n: u64) {
        self.files_processed.fetch_add(n, Ordering::Relaxed);
    }

    /// Byte pairs counted so far.
    #[must_use]
    pub fn pairs_processed(&self) -> u64 {
        self.pairs_processed.load(Ordering::Relaxed)
    }

    /// Decompressed text bytes counted so far.
    #[must_use]
    pub fn bytes_processed(&self) -> u64 {
        self.bytes_processed.load(Ordering::Relaxed)
    }

    /// Files or shards completed so far.
    #[must_use]
    pub fn files_processed(&self) -> u64 {
        self.files_processed.load(Ordering::Relaxed)
    }

    /// Current count for one byte pair.
    #[must_use]
    pub fn count(&self, c1: u8, c2: u8) -> u64 {
        self.counts[LearnSettings::pair_index(c1, c2)].load(Ordering::Relaxed)
    }

    /// All pair counts as little-endian `u64` bytes for checkpointing.
    #[must_use]
    pub fn snapshot(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(LearnSettings::SNAPSHOT_BYTES);
        for count in self.counts.iter().map(|c| c.load(Ordering::Relaxed)) {
            out.extend_from_slice(&count.to_le_bytes());
        }
        out
    }

    /// Restore a checkpoint into a fresh counter.
    ///
    /// # Errors
    ///
    /// Returns [`LearnError`] when the snapshot length is wrong or this counter
    /// already contains data.
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
        for (idx, chunk) in snapshot.chunks_exact(8).enumerate() {
            let mut bytes = [0; 8];
            bytes.copy_from_slice(chunk);
            let n = u64::from_le_bytes(bytes);
            if n > 0 {
                self.add_pair_by_index(idx, n);
            }
        }
        self.pairs_processed.store(pairs, Ordering::Relaxed);
        self.bytes_processed.store(bytes, Ordering::Relaxed);
        self.files_processed.store(files, Ordering::Relaxed);
        Ok(())
    }

    /// Serialize the learned weight table in the `SPNG` binary format.
    #[must_use]
    pub fn to_table_bytes(&self) -> Vec<u8> {
        self.weight_table(Tuning::OFF).to_bytes()
    }

    fn is_fresh(&self) -> bool {
        self.pairs_processed() == 0
            && self.bytes_processed() == 0
            && self.files_processed() == 0
            && self.counts.iter().all(|c| c.load(Ordering::Relaxed) == 0)
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

    fn weight_table(&self, tuning: Tuning) -> WeightTable {
        let total = self.pairs_processed();
        WeightTable::from_weight_fn(|c1, c2| {
            let raw = compute_weight(total, self.count(c1, c2));
            tune_weight(raw, c1, c2, tuning)
        })
    }

    #[cfg(test)]
    fn mint_table_bytes(
        &self,
        options: &mint::MintOptions<'_>,
    ) -> Result<Vec<u8>, sngram_types::TableError> {
        Ok(self
            .weight_table(options.tuning)
            .with_provenance(options.provenance)?
            .to_bytes())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use sngram_types::WeightTable;

    use super::*;
    use crate::learn::mint::{MintOptions, Tuning, is_boundary_pair};

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

    #[test]
    fn mint_round_trips_version_and_provenance() {
        let counter = counter_with_corpus();
        let options = MintOptions {
            provenance: "corpus=fs-validate;date=2026-07-03;commit=deadbeef",
            tuning: Tuning::default(),
        };
        let table = WeightTable::from_bytes(&counter.mint_table_bytes(&options).unwrap()).unwrap();

        assert_eq!(table.version(), 2);
        assert_eq!(table.provenance(), Some(options.provenance));
    }

    #[test]
    fn mint_rejects_oversized_provenance() {
        let counter = BigramCounter::new();
        let big = "x".repeat(2048);
        let options = MintOptions {
            provenance: &big,
            tuning: Tuning::OFF,
        };

        assert!(counter.mint_table_bytes(&options).is_err());
    }

    #[test]
    fn identity_tuning_matches_v1_weights() {
        let counter = counter_with_corpus();
        let v1 = WeightTable::from_bytes(&counter.to_table_bytes()).unwrap();
        let options = MintOptions {
            provenance: "p",
            tuning: Tuning::OFF,
        };
        let v2 = WeightTable::from_bytes(&counter.mint_table_bytes(&options).unwrap()).unwrap();

        for a in [b'_', b's', b'c', b'\n', b'.', b'k'] {
            for b in [b'_', b's', b'c', b'\n', b'.', b'k'] {
                assert_eq!(v1.weight(a, b), v2.weight(a, b), "({a},{b})");
            }
        }
    }

    #[test]
    fn boundary_pairs_discount_toward_floor() {
        let counter = counter_with_corpus();
        let tuning = Tuning {
            boundary_discount: 16,
            boundary_floor: 1,
        };
        let v1 = WeightTable::from_bytes(&counter.to_table_bytes()).unwrap();
        let options = MintOptions {
            provenance: "p",
            tuning,
        };
        let v2 = WeightTable::from_bytes(&counter.mint_table_bytes(&options).unwrap()).unwrap();

        for (a, b) in [
            (b'd', b'_'),
            (b'_', b'c'),
            (b'e', b'.'),
            (b'.', b'r'),
            (b'1', b'-'),
            (b'c', b':'),
            (b't', b'\n'),
            (b'\n', b's'),
        ] {
            let expected = (v1.weight(a, b) / 16).max(1);
            assert_eq!(v2.weight(a, b), expected, "boundary pair ({a},{b})");
        }
    }

    #[test]
    fn case_seam_and_interior_pairs_are_classified_correctly() {
        assert!(is_boundary_pair(b'd', b'C'));
        assert!(!is_boundary_pair(b'D', b'c'));
        assert!(!is_boundary_pair(b'D', b'C'));
        assert!(!is_boundary_pair(b'd', b'c'));
    }

    #[test]
    fn interior_pairs_pass_through_tuned_mint() {
        let counter = counter_with_corpus();
        let v1 = WeightTable::from_bytes(&counter.to_table_bytes()).unwrap();
        let options = MintOptions {
            provenance: "p",
            tuning: Tuning::default(),
        };
        let v2 = WeightTable::from_bytes(&counter.mint_table_bytes(&options).unwrap()).unwrap();

        for (a, b) in [(b's', b'c'), (b'c', b'h'), (b'o', b'c'), (b'z', b'q')] {
            assert_eq!(v1.weight(a, b), v2.weight(a, b), "interior pair ({a},{b})");
        }
    }

    fn counter_with_corpus() -> BigramCounter {
        let counter = BigramCounter::new();
        for _ in 0..50 {
            counter.process(b"sched_clock init\nsched_boost done\nmodule.rs v1.2-rc:3");
        }
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
