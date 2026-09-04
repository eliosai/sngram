//! What one scan produces: keyed grams with their spans and the final summary

use core::ops::Range;

use super::flags::ScanFlags;
use crate::{ByteSet256, EdgeBytes, SaturatingByteCounts256};

/// Final sparse-gram index key emitted by the scanner
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GramKey(u64);

impl GramKey {
    /// A key from its raw 64-bit value
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw 64-bit key value
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Byte span in the original scanned content
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ByteRange {
    /// Inclusive start byte offset
    pub start: usize,
    /// Exclusive end byte offset
    pub end: usize,
}

impl ByteRange {
    /// A span from its start and end offsets
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// The span as a standard range
    #[must_use]
    pub const fn as_range(self) -> Range<usize> {
        self.start..self.end
    }
}

/// One sparse n-gram key emitted by [`scan`](crate::scan)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScannedGram {
    /// The index key to store, which may cover virtual sentinels or a case-folded twin of `span`
    pub key: GramKey,
    /// The related span in the original content, with virtual sentinels removed
    pub span: ByteRange,
}

impl ScannedGram {
    /// A gram from its key and content span
    #[must_use]
    pub const fn new(key: GramKey, span: ByteRange) -> Self {
        Self { key, span }
    }

    /// The span in the original content as a standard range
    #[must_use]
    pub const fn content_span(&self) -> Range<usize> {
        self.span.as_range()
    }
}

/// Final scan-derived metadata for one indexed text entry
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ScanSummary {
    /// Original content length in bytes
    pub byte_len: u64,
    /// Number of text lines observed
    pub line_count: u32,
    /// Number of empty lines observed
    pub empty_line_count: u32,
    /// Longest line length in bytes, excluding the newline
    pub longest_line_len: u32,
    /// Number of gram keys emitted
    pub gram_count: u32,
    /// Boolean scan facts
    pub flags: ScanFlags,
    /// Saturating byte histogram
    pub byte_counts: SaturatingByteCounts256,
    /// First byte seen on each line
    pub line_start_bytes: ByteSet256,
    /// Last byte seen before each line break or the end of the content
    pub line_end_bytes: ByteSet256,
    /// First bytes of the content
    pub prefix: EdgeBytes,
    /// Last bytes of the content
    pub suffix: EdgeBytes,
}

#[cfg(test)]
mod tests {
    use super::{ByteRange, GramKey, ScannedGram};

    #[test]
    fn gram_span_is_range() {
        let gram = ScannedGram::new(GramKey::new(7), ByteRange::new(1, 4));

        assert_eq!(gram.content_span(), 1..4);
        assert_eq!(gram.key.value(), 7);
    }
}
