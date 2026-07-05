//! Bigram counting for weight-table learning.
//!
//! Feed text through a [`LocalTally`] (single-threaded, plain `u32` counts),
//! merge tallies into a shared [`BigramCounter`] (lock-free, written
//! concurrently by all workers), then serialize the learned table with
//! [`BigramCounter::to_table_bytes`]. The output parses with
//! [`sngram_types::WeightTable::from_bytes`].
//!
//! Counting is per-value: no bigram may straddle two inputs, so the learned
//! table is a function of the data alone, not of batch geometry.

use std::sync::atomic::{AtomicU64, Ordering};

use sngram_types::{TableError, WeightTable};

const PAIR_COUNT: usize = 256 * 256;

/// Shared byte-pair frequency counter, written concurrently by all workers.
///
/// Beyond the bigram table it aggregates run statistics: `bytes_processed` is
/// decompressed text counted, `downloaded_bytes` is compressed bytes pulled
/// over the network, `files_processed` is completed files.
pub struct BigramCounter {
    counts: Box<[AtomicU64; PAIR_COUNT]>,
    pairs_processed: AtomicU64,
    bytes_processed: AtomicU64,
    downloaded_bytes: AtomicU64,
    files_processed: AtomicU64,
}

/// Per-batch accumulator using plain `u32` counts (no atomics).
///
/// Merged into the shared [`BigramCounter`] once per batch, keeping the
/// hot counting loop free of atomic contention.
pub struct LocalTally {
    counts: Box<[u32; PAIR_COUNT]>,
    pairs: u64,
    bytes: u64,
}

impl Default for LocalTally {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalTally {
    /// Fresh tally with all counts zero.
    #[must_use]
    #[allow(
        clippy::expect_used,
        clippy::missing_panics_doc,
        reason = "Vec has exactly PAIR_COUNT elements; cannot fail"
    )]
    pub fn new() -> Self {
        let counts = vec![0u32; PAIR_COUNT]
            .into_boxed_slice()
            .try_into()
            .expect("PAIR_COUNT elements");
        Self {
            counts,
            pairs: 0,
            bytes: 0,
        }
    }

    /// Count every overlapping byte pair in one value's bytes.
    ///
    /// The rolling index (carry the previous byte, shift-or the next) measures
    /// ~11–22% faster than a `windows(2)` loop: one load per byte instead of a
    /// two-byte slice view. K-way split accumulators were measured a NET LOSS
    /// here — the bottleneck is histogram load latency, not the increment
    /// dependency — so this stays a single table.
    pub fn count_buffer(&mut self, buf: &[u8]) {
        self.bytes += buf.len() as u64;
        let [first, rest @ ..] = buf else { return };
        if rest.is_empty() {
            return;
        }
        let counts = &mut *self.counts;
        let mut hi = usize::from(*first) << 8;
        for &b in rest {
            let lo = usize::from(b);
            counts[hi | lo] += 1;
            hi = lo << 8;
        }
        self.pairs += (buf.len() - 1) as u64;
    }

    /// Total bytes counted so far.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Merge another tally's counts into this one — used when decoding
    /// batches in a background task and folding the result back into
    /// a parent tally to preserve exactly-once semantics.
    pub fn add_from(&mut self, other: &Self) {
        for i in 0..PAIR_COUNT {
            self.counts[i] = self.counts[i].saturating_add(other.counts[i]);
        }
        self.pairs += other.pairs;
        self.bytes += other.bytes;
    }
}

impl Default for BigramCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl BigramCounter {
    /// Fresh counter with all counts zero.
    #[must_use]
    #[allow(
        clippy::expect_used,
        clippy::missing_panics_doc,
        reason = "exactly PAIR_COUNT elements collected; cannot fail"
    )]
    pub fn new() -> Self {
        let counts: Box<[AtomicU64; PAIR_COUNT]> = (0..PAIR_COUNT)
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice()
            .try_into()
            .expect("PAIR_COUNT elements collected");
        Self {
            counts,
            pairs_processed: AtomicU64::new(0),
            bytes_processed: AtomicU64::new(0),
            downloaded_bytes: AtomicU64::new(0),
            files_processed: AtomicU64::new(0),
        }
    }

    /// Fold a tally's counts and totals into the shared counter.
    pub fn merge(&self, tally: &LocalTally) {
        for (idx, &n) in tally.counts.iter().enumerate() {
            if n > 0 {
                self.counts[idx].fetch_add(u64::from(n), Ordering::Relaxed);
            }
        }
        self.pairs_processed
            .fetch_add(tally.pairs, Ordering::Relaxed);
        self.bytes_processed
            .fetch_add(tally.bytes, Ordering::Relaxed);
    }

    /// Count one value's bytes directly (convenience over a one-shot tally).
    pub fn process(&self, content: &[u8]) {
        let mut tally = LocalTally::new();
        tally.count_buffer(content);
        self.merge(&tally);
    }

    /// Record `n` completed files.
    pub fn inc_files(&self, n: u64) {
        self.files_processed.fetch_add(n, Ordering::Relaxed);
    }

    /// Record `n` compressed bytes downloaded.
    pub fn add_downloaded(&self, n: u64) {
        self.downloaded_bytes.fetch_add(n, Ordering::Relaxed);
    }

    /// Compressed bytes downloaded so far.
    #[must_use]
    pub fn downloaded_bytes(&self) -> u64 {
        self.downloaded_bytes.load(Ordering::Relaxed)
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

    /// Files completed so far.
    #[must_use]
    pub fn files_processed(&self) -> u64 {
        self.files_processed.load(Ordering::Relaxed)
    }

    /// Current count for one byte pair.
    #[must_use]
    pub fn count(&self, c1: u8, c2: u8) -> u64 {
        let idx = usize::from(c1) << 8 | usize::from(c2);
        self.counts[idx].load(Ordering::Relaxed)
    }

    /// Snapshot all `PAIR_COUNT` counts in index order — for checkpointing.
    #[must_use]
    pub fn counts_vec(&self) -> Vec<u64> {
        self.counts
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect()
    }

    /// Add `n` to one byte pair's count — for checkpoint restore.
    pub fn add(&self, c1: u8, c2: u8, n: u64) {
        let idx = usize::from(c1) << 8 | usize::from(c2);
        self.counts[idx].fetch_add(n, Ordering::Relaxed);
    }

    /// Add `n` to the pair total — for checkpoint restore.
    pub fn add_pairs(&self, n: u64) {
        self.pairs_processed.fetch_add(n, Ordering::Relaxed);
    }

    /// Add `n` to the byte total — for checkpoint restore.
    pub fn add_bytes(&self, n: u64) {
        self.bytes_processed.fetch_add(n, Ordering::Relaxed);
    }

    /// Add `n` to the file total — for checkpoint restore.
    pub fn add_files(&self, n: u64) {
        self.files_processed.fetch_add(n, Ordering::Relaxed);
    }

    /// Serialize the learned weight table (weight = `total_pairs / count`,
    /// `u32::MAX` for unseen pairs) in the `SPNG` binary format that
    /// [`sngram_types::WeightTable::from_bytes`] loads.
    #[must_use]
    pub fn to_table_bytes(&self) -> Vec<u8> {
        self.weight_table(Tuning::OFF).to_bytes()
    }

    /// Mint a v2 table: tuned weights plus an embedded provenance record.
    ///
    /// # Errors
    ///
    /// Returns [`TableError::InvalidProvenance`] when the provenance record
    /// is too large for the table format.
    pub fn mint_table_bytes(&self, spec: &MintSpec<'_>) -> Result<Vec<u8>, TableError> {
        Ok(self
            .weight_table(spec.tuning)
            .with_provenance(spec.provenance)?
            .to_bytes())
    }

    fn weight_table(&self, tuning: Tuning) -> WeightTable {
        let total = self.pairs_processed();
        WeightTable::from_weight_fn(|c1, c2| {
            let idx = usize::from(c1) << 8 | usize::from(c2);
            let raw = compute_weight(total, self.counts[idx].load(Ordering::Relaxed));
            tune_weight(raw, c1, c2, tuning)
        })
    }
}

/// Everything a v2 mint embeds and applies beyond the raw counts.
#[derive(Debug, Clone)]
pub struct MintSpec<'a> {
    /// Provenance record, freeform UTF-8 (corpus, date, commit).
    pub provenance: &'a str,
    /// Boundary-pair discounts shaping gram geometry.
    pub tuning: Tuning,
}

/// Boundary-pair weight discounts applied at mint time.
///
/// Pairs touching identifier separators (`_ . / - :`), the lowercase-to-
/// uppercase case seam, or a line terminator get their weight divided by
/// `boundary_discount` (never below `boundary_floor`), landing them interior
/// to grams: compound identifiers and line edges yield bridging grams the
/// planner can demand, killing the trigram-scatter and anchored FP classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tuning {
    /// Divisor applied to boundary-class pair weights; 1 disables.
    pub boundary_discount: u32,
    /// Lowest weight a discount may produce.
    pub boundary_floor: u32,
}

impl Tuning {
    /// Identity tuning: weights pass through unchanged.
    pub const OFF: Self = Self {
        boundary_discount: 1,
        boundary_floor: 1,
    };
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            boundary_discount: 16,
            boundary_floor: 1,
        }
    }
}

/// Whether a pair sits on a boundary the tuning discounts into a valley.
#[must_use]
pub const fn is_boundary_pair(c1: u8, c2: u8) -> bool {
    is_separator(c1)
        || is_separator(c2)
        || is_line_terminator(c1)
        || is_line_terminator(c2)
        || (c1.is_ascii_lowercase() && c2.is_ascii_uppercase())
}

/// Separator bytes that split compound identifiers and paths.
const fn is_separator(c: u8) -> bool {
    matches!(c, b'_' | b'.' | b'/' | b'-' | b':')
}

const fn is_line_terminator(c: u8) -> bool {
    matches!(c, b'\n' | b'\r')
}

const fn tune_weight(raw: u32, c1: u8, c2: u8, tuning: Tuning) -> u32 {
    if tuning.boundary_discount <= 1 || !is_boundary_pair(c1, c2) {
        return raw;
    }
    let discounted = raw / tuning.boundary_discount;
    if discounted < tuning.boundary_floor {
        tuning.boundary_floor
    } else {
        discounted
    }
}

#[allow(clippy::cast_possible_truncation, reason = "min() clamps to u32::MAX")]
fn compute_weight(total: u64, count: u64) -> u32 {
    if count == 0 {
        return u32::MAX;
    }
    (total / count).min(u64::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use sngram_types::WeightTable;

    #[test]
    fn empty_counter_produces_valid_table() {
        let c = BigramCounter::new();
        let table = WeightTable::from_bytes(&c.to_table_bytes()).unwrap();
        assert_eq!(table.weight(0, 0), u32::MAX);
    }

    #[test]
    fn counts_byte_pairs() {
        let c = BigramCounter::new();
        c.process(b"aab");
        assert_eq!(c.count(b'a', b'a'), 1);
        assert_eq!(c.count(b'a', b'b'), 1);
        assert_eq!(c.count(b'b', b'a'), 0);
    }

    #[test]
    fn tracks_pairs_and_bytes() {
        let c = BigramCounter::new();
        c.process(b"hello");
        c.process(b"world");
        assert_eq!(c.pairs_processed(), 8);
        assert_eq!(c.bytes_processed(), 10);
    }

    #[test]
    fn files_tracked_separately() {
        let c = BigramCounter::new();
        c.inc_files(1);
        c.inc_files(1);
        assert_eq!(c.files_processed(), 2);
    }

    #[test]
    fn no_bigram_straddles_a_value_boundary() {
        // Counting values separately (as the pipeline does, one per row) must
        // never fabricate a bigram across the boundary. This is the invariant
        // whose violation (whole-buffer counting) made the table depend on
        // batch geometry and broke determinism.
        let c = BigramCounter::new();
        c.process(b"ab");
        c.process(b"cd");
        assert_eq!(c.count(b'b', b'c'), 0, "no bigram may straddle a boundary");
        assert_eq!(c.count(b'a', b'b'), 1);
        assert_eq!(c.count(b'c', b'd'), 1);
        // Per-value pair total excludes the phantom boundary pair.
        assert_eq!(c.pairs_processed(), 2);
    }

    #[test]
    fn counting_is_independent_of_value_grouping() {
        // However rows are split into batches/row groups, per-value counts sum
        // identically — the table is a function of the data, not the layout.
        let one = BigramCounter::new();
        one.process(b"aa");
        one.process(b"aa");
        one.process(b"aa");

        let split = BigramCounter::new();
        for _ in 0..3 {
            split.process(b"aa");
        }

        assert_eq!(one.count(b'a', b'a'), split.count(b'a', b'a'));
        assert_eq!(one.count(b'a', b'a'), 3);
    }

    #[test]
    fn tally_merge_matches_direct() {
        let c = BigramCounter::new();
        let mut t = LocalTally::new();
        let data = b"the quick brown fox jumps over the lazy dog";
        t.count_buffer(data);
        c.merge(&t);

        let direct = BigramCounter::new();
        for pair in data.windows(2) {
            direct.add(pair[0], pair[1], 1);
        }
        for a in 0u8..=255 {
            for b in 0u8..=255 {
                assert_eq!(c.count(a, b), direct.count(a, b), "mismatch at ({a},{b})");
            }
        }
    }

    #[test]
    fn tally_accumulates_across_buffers() {
        let c = BigramCounter::new();
        let mut t = LocalTally::new();
        t.count_buffer(b"ab");
        t.count_buffer(b"ab");
        c.merge(&t);
        assert_eq!(c.count(b'a', b'b'), 2);
        assert_eq!(c.pairs_processed(), 2);
        assert_eq!(c.bytes_processed(), 4);
    }

    #[test]
    fn tally_counts_boundary_byte_pair_indices() {
        let c = BigramCounter::new();
        let mut tally = LocalTally::new();
        tally.count_buffer(&[0, u8::MAX, 0]);
        c.merge(&tally);

        assert_eq!(c.count(0, u8::MAX), 1);
        assert_eq!(c.count(u8::MAX, 0), 1);
        assert_eq!(c.pairs_processed(), 2);
        assert_eq!(c.bytes_processed(), 3);
    }

    #[test]
    fn pair_index_edges_restore_and_serialize() {
        let c = BigramCounter::new();
        for (left, right, count) in [
            (0u8, 0u8, 3u64),
            (0, u8::MAX, 7),
            (u8::MAX, 0, 11),
            (u8::MAX, u8::MAX, 5),
        ] {
            c.add(left, right, count);
        }
        c.add_pairs(26);

        assert_eq!(c.count(0, 0), 3);
        assert_eq!(c.count(0, u8::MAX), 7);
        assert_eq!(c.count(u8::MAX, 0), 11);
        assert_eq!(c.count(u8::MAX, u8::MAX), 5);

        let table = WeightTable::from_bytes(&c.to_table_bytes()).unwrap();
        assert_eq!(table.weight(0, 0), 26 / 3);
        assert_eq!(table.weight(0, u8::MAX), 26 / 7);
        assert_eq!(table.weight(u8::MAX, 0), 26 / 11);
        assert_eq!(table.weight(u8::MAX, u8::MAX), 26 / 5);
    }

    #[test]
    fn frequent_pairs_get_lower_weight() {
        let c = BigramCounter::new();
        for _ in 0..100 {
            c.process(b"the quick brown fox");
        }
        c.process(b"zqzq");
        let table = WeightTable::from_bytes(&c.to_table_bytes()).unwrap();
        let common = table.weight(b't', b'h');
        let rare = table.weight(b'z', b'q');
        assert!(rare > common, "rare={rare} should be > common={common}");
    }

    // The learning rule has no reference *code* (danlark1 uses a fixed hash, not
    // a learned table); its only reference is the article's rule: weight =
    // 1/frequency. This pins the whole count -> weight chain to an independent,
    // obviously-correct implementation of exactly that rule, weight for weight.
    fn reference_counts(corpus: &[&[u8]]) -> (std::collections::HashMap<(u8, u8), u64>, u64) {
        let mut counts = std::collections::HashMap::new();
        let mut total: u64 = 0;
        for row in corpus {
            for w in row.windows(2) {
                *counts.entry((w[0], w[1])).or_default() += 1;
                total += 1;
            }
        }
        (counts, total)
    }

    fn table_for_corpus(corpus: &[&[u8]]) -> (BigramCounter, WeightTable) {
        let c = BigramCounter::new();
        for row in corpus {
            c.process(row);
        }
        let table = WeightTable::from_bytes(&c.to_table_bytes()).unwrap();
        (c, table)
    }

    fn assert_table_matches_reference(
        table: &WeightTable,
        counts: &std::collections::HashMap<(u8, u8), u64>,
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

    #[test]
    fn learned_table_matches_independent_reference() {
        let corpus: &[&[u8]] = &[
            b"fn main() { let x = 42; }",
            b"the quick brown fox jumps over the lazy dog",
            b"SELECT * FROM users WHERE id = 1;",
            b"\x00\x01\x02\xc8\xff\xfe\x00\x01",
        ];
        let (counts, total) = reference_counts(corpus);
        let (c, table) = table_for_corpus(corpus);
        assert_eq!(
            c.pairs_processed(),
            total,
            "pair total must match reference"
        );
        assert_table_matches_reference(&table, &counts, total);
    }

    #[allow(clippy::cast_possible_truncation, reason = "min() clamps to u32 range")]
    fn expected_weight(total: u64, count: u64) -> u32 {
        total
            .checked_div(count)
            .map_or(u32::MAX, |w| w.min(u64::from(u32::MAX)) as u32)
    }

    fn tally_ab(n: usize) -> LocalTally {
        let mut t = LocalTally::new();
        for _ in 0..n {
            t.count_buffer(b"ab");
        }
        t
    }

    #[test]
    fn concurrent_merge_is_deterministic() {
        let c = std::sync::Arc::new(BigramCounter::new());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let c = c.clone();
                std::thread::spawn(move || c.merge(&tally_ab(1000)))
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(c.count(b'a', b'b'), 8000);
    }

    #[test]
    fn short_content_skipped() {
        let c = BigramCounter::new();
        c.process(b"");
        c.process(b"x");
        assert_eq!(c.pairs_processed(), 0);
    }

    fn counter_with_corpus() -> BigramCounter {
        let c = BigramCounter::new();
        for _ in 0..50 {
            c.process(b"sched_clock init\nsched_boost done\nmodule.rs v1.2-rc:3");
        }
        c
    }

    #[test]
    fn mint_round_trips_version_and_provenance() {
        let c = counter_with_corpus();
        let spec = MintSpec {
            provenance: "corpus=fs-validate;date=2026-07-03;commit=deadbeef",
            tuning: Tuning::default(),
        };
        let table = WeightTable::from_bytes(&c.mint_table_bytes(&spec).unwrap()).unwrap();
        assert_eq!(table.version(), 2);
        assert_eq!(table.provenance(), Some(spec.provenance));
    }

    #[test]
    fn mint_rejects_oversized_provenance() {
        let c = BigramCounter::new();
        let big = "x".repeat(2048);
        let spec = MintSpec {
            provenance: &big,
            tuning: Tuning::OFF,
        };
        assert!(c.mint_table_bytes(&spec).is_err());
    }

    #[test]
    fn identity_tuning_matches_v1_weights() {
        let c = counter_with_corpus();
        let v1 = WeightTable::from_bytes(&c.to_table_bytes()).unwrap();
        let spec = MintSpec {
            provenance: "p",
            tuning: Tuning::OFF,
        };
        let v2 = WeightTable::from_bytes(&c.mint_table_bytes(&spec).unwrap()).unwrap();
        for a in [b'_', b's', b'c', b'\n', b'.', b'k'] {
            for b in [b'_', b's', b'c', b'\n', b'.', b'k'] {
                assert_eq!(v1.weight(a, b), v2.weight(a, b), "({a},{b})");
            }
        }
    }

    #[test]
    fn boundary_pairs_discount_toward_floor() {
        let c = counter_with_corpus();
        let tuning = Tuning {
            boundary_discount: 16,
            boundary_floor: 1,
        };
        let v1 = WeightTable::from_bytes(&c.to_table_bytes()).unwrap();
        let spec = MintSpec {
            provenance: "p",
            tuning,
        };
        let v2 = WeightTable::from_bytes(&c.mint_table_bytes(&spec).unwrap()).unwrap();
        let separators = [
            (b'd', b'_'),
            (b'_', b'c'),
            (b'e', b'.'),
            (b'.', b'r'),
            (b'1', b'-'),
            (b'c', b':'),
        ];
        for (a, b) in separators {
            let expected = (v1.weight(a, b) / 16).max(1);
            assert_eq!(v2.weight(a, b), expected, "separator pair ({a},{b})");
        }
    }

    #[test]
    fn newline_pairs_discount_on_both_sides() {
        let c = counter_with_corpus();
        let v1 = WeightTable::from_bytes(&c.to_table_bytes()).unwrap();
        let spec = MintSpec {
            provenance: "p",
            tuning: Tuning::default(),
        };
        let v2 = WeightTable::from_bytes(&c.mint_table_bytes(&spec).unwrap()).unwrap();
        for (a, b) in [(b't', b'\n'), (b'\n', b's'), (b'x', b'\r'), (b'\r', b'x')] {
            let expected = (v1.weight(a, b) / 16).max(1);
            assert_eq!(v2.weight(a, b), expected, "terminator pair ({a},{b})");
        }
    }

    #[test]
    fn case_seam_discounts_lower_to_upper_only() {
        assert!(is_boundary_pair(b'd', b'C'));
        assert!(!is_boundary_pair(b'D', b'c'));
        assert!(!is_boundary_pair(b'D', b'C'));
        assert!(!is_boundary_pair(b'd', b'c'));
    }

    #[test]
    fn interior_pairs_pass_through_untuned() {
        let c = counter_with_corpus();
        let v1 = WeightTable::from_bytes(&c.to_table_bytes()).unwrap();
        let spec = MintSpec {
            provenance: "p",
            tuning: Tuning::default(),
        };
        let v2 = WeightTable::from_bytes(&c.mint_table_bytes(&spec).unwrap()).unwrap();
        for (a, b) in [(b's', b'c'), (b'c', b'h'), (b'o', b'c'), (b'z', b'q')] {
            assert_eq!(v1.weight(a, b), v2.weight(a, b), "interior pair ({a},{b})");
        }
    }
}
