//! Byte presence, count, and edge types for scan metadata.

/// Exact set of byte values present in scanned text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ByteSet256 {
    /// Four 64-bit words, one bit per byte value.
    pub words: [u64; 4],
}

impl ByteSet256 {
    /// Add one byte to the set.
    pub fn insert(&mut self, byte: u8) {
        let idx = usize::from(byte);
        self.words[idx / 64] |= 1u64 << (idx % 64);
    }

    /// True when every byte in `need` is present.
    #[must_use]
    pub const fn contains_all(self, need: Self) -> bool {
        (self.words[0] & need.words[0]) == need.words[0]
            && (self.words[1] & need.words[1]) == need.words[1]
            && (self.words[2] & need.words[2]) == need.words[2]
            && (self.words[3] & need.words[3]) == need.words[3]
    }

    /// True when at least one byte in `need` is present.
    #[must_use]
    pub const fn contains_any(self, need: Self) -> bool {
        (self.words[0] & need.words[0]) != 0
            || (self.words[1] & need.words[1]) != 0
            || (self.words[2] & need.words[2]) != 0
            || (self.words[3] & need.words[3]) != 0
    }

    /// True when the set is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.words[0] == 0 && self.words[1] == 0 && self.words[2] == 0 && self.words[3] == 0
    }
}

/// Saturating byte histogram for scanned text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SaturatingByteCounts256 {
    /// One saturating count per byte value; `u8::MAX` means at least 255.
    pub counts: [u8; 256],
}

impl Default for SaturatingByteCounts256 {
    fn default() -> Self {
        Self { counts: [0; 256] }
    }
}

impl SaturatingByteCounts256 {
    /// Count one byte, saturating at `u8::MAX`.
    pub fn observe(&mut self, byte: u8) {
        let slot = &mut self.counts[usize::from(byte)];
        *slot = slot.saturating_add(1);
    }

    /// True when this histogram proves all required minimum byte counts.
    ///
    /// A saturated stored count is treated as satisfying any `u8` requirement;
    /// callers must only encode requirements up to `u8::MAX`.
    #[must_use]
    pub fn contains_at_least(&self, need: &Self) -> bool {
        self.counts
            .iter()
            .zip(need.counts)
            .all(|(&have, req)| have >= req || have == u8::MAX)
    }

    /// True when all requirements are zero.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.counts.iter().all(|&count| count == 0)
    }
}

/// Fixed-size content edge bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct EdgeBytes {
    /// Number of valid bytes in `bytes`.
    len: u8,
    /// Edge bytes, padded with zeros.
    bytes: [u8; 16],
}

impl EdgeBytes {
    /// Maximum edge bytes stored.
    pub const CAPACITY: usize = 16;

    /// Store the leading bytes of `bytes`, truncated to [`Self::CAPACITY`].
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

    /// Append one byte when capacity remains.
    pub const fn push(&mut self, byte: u8) {
        let len = self.len();
        if len < Self::CAPACITY {
            self.bytes[len] = byte;
            self.len = self.len.saturating_add(1);
        }
    }

    /// Number of edge bytes stored.
    #[must_use]
    pub const fn len(self) -> usize {
        let len = self.len as usize;
        if len > Self::CAPACITY {
            Self::CAPACITY
        } else {
            len
        }
    }

    /// True when the edge has no bytes.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// The valid edge bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_set_tracks_membership() {
        let mut set = ByteSet256::default();
        let mut need = ByteSet256::default();
        set.insert(b'a');
        set.insert(b'z');
        need.insert(b'z');

        assert!(set.contains_all(need));
        assert!(set.contains_any(need));
    }

    #[test]
    fn saturating_counts_track_minimums() {
        let mut have = SaturatingByteCounts256::default();
        let mut need = SaturatingByteCounts256::default();
        have.observe(b'a');
        have.observe(b'a');
        need.observe(b'a');

        assert!(have.contains_at_least(&need));
    }
}
