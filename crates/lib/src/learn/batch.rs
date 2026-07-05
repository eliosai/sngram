//! Per-batch byte-pair counts.

use super::settings::LearnSettings;

/// Per-batch accumulator using plain `u32` counts.
///
/// Merge batch counts into [`super::BigramCounter`] only after the batch or row
/// group has completed. This keeps the hot counting loop free of atomic
/// contention and preserves exactly-once retry semantics.
pub struct BatchCounts {
    counts: Box<[u32; LearnSettings::PAIR_COUNT]>,
    pairs: u64,
    bytes: u64,
}

impl Default for BatchCounts {
    fn default() -> Self {
        Self::new()
    }
}

impl BatchCounts {
    /// Fresh batch with all counts zero.
    #[must_use]
    pub fn new() -> Self {
        let counts = vec![0u32; LearnSettings::PAIR_COUNT]
            .into_boxed_slice()
            .try_into()
            .unwrap_or_else(|_| unreachable!("pair-count elements"));
        Self {
            counts,
            pairs: 0,
            bytes: 0,
        }
    }

    /// Count every overlapping byte pair in one value's bytes.
    ///
    /// The value boundary is semantic: no pair is counted across calls.
    pub fn count_buffer(&mut self, buf: &[u8]) {
        self.bytes = self.bytes.saturating_add(buf.len() as u64);
        let [first, rest @ ..] = buf else { return };
        if rest.is_empty() {
            return;
        }
        let mut previous = *first;
        for &b in rest {
            let idx = LearnSettings::pair_index(previous, b);
            self.counts[idx] = self.counts[idx].saturating_add(1);
            previous = b;
        }
        self.pairs = self.pairs.saturating_add((buf.len() - 1) as u64);
    }

    /// Text bytes counted so far.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn pair_counts(&self) -> &[u32; LearnSettings::PAIR_COUNT] {
        &self.counts
    }

    pub const fn pairs_counted(&self) -> u64 {
        self.pairs
    }

    #[cfg(test)]
    fn count(&self, c1: u8, c2: u8) -> u32 {
        self.counts[LearnSettings::pair_index(c1, c2)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_bigram_straddles_value_boundaries() {
        let mut batch = BatchCounts::new();
        batch.count_buffer(b"ab");
        batch.count_buffer(b"cd");

        assert_eq!(batch.count(b'a', b'b'), 1);
        assert_eq!(batch.count(b'b', b'c'), 0);
        assert_eq!(batch.count(b'c', b'd'), 1);
        assert_eq!(batch.pairs, 2);
    }

    #[test]
    fn boundary_pair_indices_are_counted() {
        let mut batch = BatchCounts::new();
        batch.count_buffer(&[0, u8::MAX, 0]);

        assert_eq!(batch.count(0, u8::MAX), 1);
        assert_eq!(batch.count(u8::MAX, 0), 1);
        assert_eq!(batch.pairs, 2);
        assert_eq!(batch.bytes(), 3);
    }
}
