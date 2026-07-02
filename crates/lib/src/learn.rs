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

use sngram_types::TABLE_BINARY_SIZE;

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
    #[allow(clippy::indexing_slicing, reason = "u8<<8|u8 < 65536")]
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
    #[allow(clippy::indexing_slicing, reason = "PAIR_COUNT loop")]
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
    #[allow(clippy::indexing_slicing, reason = "PAIR_COUNT iteration")]
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
    #[allow(clippy::indexing_slicing, reason = "u8<<8|u8 < 65536")]
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
    #[allow(clippy::indexing_slicing, reason = "u8<<8|u8 < 65536")]
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
    #[allow(clippy::indexing_slicing, reason = "fixed-size buffer")]
    pub fn to_table_bytes(&self) -> Vec<u8> {
        let total = self.pairs_processed();
        let mut buf = vec![0u8; TABLE_BINARY_SIZE];
        write_header(&mut buf);
        write_weights(&self.counts, total, &mut buf);
        write_checksum(&mut buf);
        buf
    }
}

#[allow(clippy::indexing_slicing, reason = "fixed header offsets")]
fn write_header(buf: &mut [u8]) {
    buf[..4].copy_from_slice(b"SPNG");
    buf[4..8].copy_from_slice(&1u32.to_le_bytes());
}

#[allow(clippy::indexing_slicing, reason = "PAIR_COUNT * 4 fits in buf")]
fn write_weights(counts: &[AtomicU64; PAIR_COUNT], total: u64, buf: &mut [u8]) {
    let data = &mut buf[16..];
    for i in 0..PAIR_COUNT {
        let w = compute_weight(total, counts[i].load(Ordering::Relaxed));
        data[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
}

#[allow(clippy::cast_possible_truncation, reason = "min() clamps to u32::MAX")]
fn compute_weight(total: u64, count: u64) -> u32 {
    if count == 0 {
        return u32::MAX;
    }
    (total / count).min(u64::from(u32::MAX)) as u32
}

#[allow(clippy::indexing_slicing, reason = "fixed header offsets")]
fn write_checksum(buf: &mut [u8]) {
    let crc = crc32fast::hash(&buf[16..]);
    buf[8..12].copy_from_slice(&crc.to_le_bytes());
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
    #[test]
    fn learned_table_matches_independent_reference() {
        use std::collections::HashMap;
        let corpus: &[&[u8]] = &[
            b"fn main() { let x = 42; }",
            b"the quick brown fox jumps over the lazy dog",
            b"SELECT * FROM users WHERE id = 1;",
            b"\x00\x01\x02\xc8\xff\xfe\x00\x01",
        ];
        let mut counts: HashMap<(u8, u8), u64> = HashMap::new();
        let mut total: u64 = 0;
        for row in corpus {
            for w in row.windows(2) {
                *counts.entry((w[0], w[1])).or_default() += 1;
                total += 1;
            }
        }

        let c = BigramCounter::new();
        for row in corpus {
            c.process(row);
        }
        assert_eq!(
            c.pairs_processed(),
            total,
            "pair total must match reference"
        );

        let table = WeightTable::from_bytes(&c.to_table_bytes()).unwrap();
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
}
