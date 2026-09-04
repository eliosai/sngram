//! Byte presence, count, and edge types for scan metadata

/// Exact set of byte values present in scanned text
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ByteSet256 {
    words: [u64; 4],
}

impl ByteSet256 {
    /// A set from its four 64-bit words, one bit per byte value
    #[must_use]
    pub const fn from_words(words: [u64; 4]) -> Self {
        Self { words }
    }

    /// The four 64-bit words, one bit per byte value
    #[must_use]
    pub const fn words(self) -> [u64; 4] {
        self.words
    }

    /// Add one byte to the set
    pub const fn insert(&mut self, byte: u8) {
        let idx = byte as usize;
        self.words[idx / 64] |= 1u64 << (idx % 64);
    }

    /// True when the byte is in the set
    #[must_use]
    pub const fn contains(self, byte: u8) -> bool {
        let idx = byte as usize;
        self.words[idx / 64] >> (idx % 64) & 1 == 1
    }

    /// True when at least one byte in `need` is present
    #[must_use]
    pub const fn contains_any(self, need: Self) -> bool {
        (self.words[0] & need.words[0]) != 0
            || (self.words[1] & need.words[1]) != 0
            || (self.words[2] & need.words[2]) != 0
            || (self.words[3] & need.words[3]) != 0
    }

    /// The bytes in either set
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self {
            words: [
                self.words[0] | other.words[0],
                self.words[1] | other.words[1],
                self.words[2] | other.words[2],
                self.words[3] | other.words[3],
            ],
        }
    }

    /// Number of bytes in the set
    #[must_use]
    pub const fn len(self) -> u32 {
        self.words[0].count_ones()
            + self.words[1].count_ones()
            + self.words[2].count_ones()
            + self.words[3].count_ones()
    }

    /// True when the set is empty
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.words[0] == 0 && self.words[1] == 0 && self.words[2] == 0 && self.words[3] == 0
    }
}

/// Saturating byte histogram for scanned text
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SaturatingByteCounts256 {
    counts: [u8; 256],
}

impl Default for SaturatingByteCounts256 {
    fn default() -> Self {
        Self { counts: [0; 256] }
    }
}

impl SaturatingByteCounts256 {
    /// A histogram from one saturating count per byte value, where `u8::MAX` means at least 255
    #[must_use]
    pub const fn from_counts(counts: [u8; 256]) -> Self {
        Self { counts }
    }

    /// One saturating count per byte value, where `u8::MAX` means at least 255
    #[must_use]
    pub const fn counts(&self) -> &[u8; 256] {
        &self.counts
    }

    /// Count one byte, saturating at `u8::MAX`
    #[inline]
    pub const fn observe(&mut self, byte: u8) {
        let slot = &mut self.counts[byte as usize];
        *slot = slot.saturating_add(1);
    }

    /// True when every count meets the minimum in `need`, where a saturated count meets any minimum
    #[must_use]
    pub fn contains_at_least(&self, need: &Self) -> bool {
        self.counts
            .iter()
            .zip(need.counts)
            .all(|(&have, req)| have >= req || have == u8::MAX)
    }

    /// True when every count is zero
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.counts.iter().all(|&count| count == 0)
    }
}

/// Fixed-size content edge bytes
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct EdgeBytes {
    len: u8,
    bytes: [u8; 16],
}

impl EdgeBytes {
    /// Maximum edge bytes stored
    pub const CAPACITY: usize = 16;

    /// The leading bytes of `bytes`, truncated to [`Self::CAPACITY`]
    #[must_use]
    pub fn from_slice(bytes: &[u8]) -> Self {
        let len = bytes.len().min(Self::CAPACITY);
        let mut out = Self {
            len: u8::try_from(len).unwrap_or(u8::MAX),
            bytes: [0; 16],
        };
        out.bytes[..len].copy_from_slice(&bytes[..len]);
        out
    }

    /// Append one byte when capacity remains
    pub const fn push(&mut self, byte: u8) {
        let len = self.len();
        if len < Self::CAPACITY {
            self.bytes[len] = byte;
            self.len = self.len.saturating_add(1);
        }
    }

    /// Number of edge bytes stored
    #[must_use]
    pub const fn len(self) -> usize {
        let len = self.len as usize;
        if len > Self::CAPACITY {
            Self::CAPACITY
        } else {
            len
        }
    }

    /// True when the edge has no bytes
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// The valid edge bytes
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len()]
    }
}

#[cfg(test)]
mod tests {
    use super::{ByteSet256, EdgeBytes, SaturatingByteCounts256};

    #[test]
    fn byte_set_tracks_membership() {
        let mut set = ByteSet256::default();
        let mut need = ByteSet256::default();
        set.insert(b'a');
        set.insert(b'z');
        need.insert(b'z');

        assert!(set.contains(b'a'));
        assert!(!set.contains(b'b'));
        assert!(set.contains_any(need));
        assert_eq!(set.union(need).len(), 2);
        assert_eq!(ByteSet256::from_words(set.words()), set);
    }

    #[test]
    fn saturating_counts_track_minimums() {
        let mut have = SaturatingByteCounts256::default();
        let mut need = SaturatingByteCounts256::default();
        have.observe(b'a');
        have.observe(b'a');
        need.observe(b'a');

        assert!(have.contains_at_least(&need));
        assert_eq!(have.counts()[usize::from(b'a')], 2);
        assert_eq!(SaturatingByteCounts256::from_counts(*have.counts()), have);
    }

    #[test]
    fn edge_bytes_keep_the_leading_capacity() {
        let edge = EdgeBytes::from_slice(b"0123456789abcdefgh");

        assert_eq!(edge.as_slice(), b"0123456789abcdef");
        assert_eq!(edge.len(), EdgeBytes::CAPACITY);
        assert!(EdgeBytes::default().is_empty());
    }
}
