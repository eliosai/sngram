//! Lock-free bigram counter for concurrent weight table learning.

use std::sync::atomic::{AtomicU64, Ordering};

use sngram_types::TABLE_BINARY_SIZE;

const PAIR_COUNT: usize = 256 * 256;

/// Counts byte-pair frequencies across concurrent threads.
pub struct BigramCounter {
    counts: Box<[AtomicU64; PAIR_COUNT]>,
    pairs_processed: AtomicU64,
    files_processed: AtomicU64,
}

impl Default for BigramCounter {
    fn default() -> Self { Self::new() }
}

impl BigramCounter {
    #[must_use]
    #[allow(clippy::expect_used, reason = "Vec has exactly PAIR_COUNT elements")]
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
            files_processed: AtomicU64::new(0),
        }
    }

    #[allow(clippy::indexing_slicing, reason = "u8<<8|u8 < 65536")]
    pub fn process(&self, content: &[u8]) {
        if content.len() < 2 { return; }
        for pair in content.windows(2) {
            let idx = usize::from(pair[0]) << 8 | usize::from(pair[1]);
            self.counts[idx].fetch_add(1, Ordering::Relaxed);
        }
        let n_pairs = content.len().saturating_sub(1) as u64;
        self.pairs_processed.fetch_add(n_pairs, Ordering::Relaxed);
        self.files_processed.fetch_add(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn pairs_processed(&self) -> u64 {
        self.pairs_processed.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn files_processed(&self) -> u64 {
        self.files_processed.load(Ordering::Relaxed)
    }

    #[must_use]
    #[allow(clippy::indexing_slicing, reason = "u8<<8|u8 < 65536")]
    pub fn count(&self, c1: u8, c2: u8) -> u64 {
        let idx = usize::from(c1) << 8 | usize::from(c2);
        self.counts[idx].load(Ordering::Relaxed)
    }

    #[allow(clippy::indexing_slicing, reason = "u8<<8|u8 < 65536")]
    pub fn add(&self, c1: u8, c2: u8, n: u64) {
        let idx = usize::from(c1) << 8 | usize::from(c2);
        self.counts[idx].fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_pairs(&self, n: u64) {
        self.pairs_processed.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_files(&self, n: u64) {
        self.files_processed.fetch_add(n, Ordering::Relaxed);
    }

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
    fn tracks_pairs_not_bytes() {
        let c = BigramCounter::new();
        c.process(b"hello");
        c.process(b"world");
        assert_eq!(c.files_processed(), 2);
        assert_eq!(c.pairs_processed(), 8);
    }

    #[test]
    fn frequent_pairs_get_lower_weight() {
        let c = BigramCounter::new();
        for _ in 0..100 { c.process(b"the quick brown fox"); }
        c.process(b"zqzq");
        let table = WeightTable::from_bytes(&c.to_table_bytes()).unwrap();
        let common = table.weight(b't', b'h');
        let rare = table.weight(b'z', b'q');
        assert!(rare > common, "rare={rare} should be > common={common}");
    }

    #[test]
    fn concurrent_processing() {
        let c = std::sync::Arc::new(BigramCounter::new());
        let handles: Vec<_> = (0..8).map(|_| {
            let c = c.clone();
            std::thread::spawn(move || {
                for _ in 0..1000 { c.process(b"ab"); }
            })
        }).collect();
        for h in handles { h.join().unwrap(); }
        assert_eq!(c.count(b'a', b'b'), 8000);
        assert_eq!(c.files_processed(), 8000);
    }

    #[test]
    fn short_content_skipped() {
        let c = BigramCounter::new();
        c.process(b"");
        c.process(b"x");
        assert_eq!(c.pairs_processed(), 0);
        assert_eq!(c.files_processed(), 0);
    }
}
