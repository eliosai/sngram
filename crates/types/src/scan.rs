//! Public scan event and metadata types.

use core::ops::Range;

use crate::{ByteSet256, EdgeBytes, SaturatingByteCounts256};

/// Final sparse-gram index key emitted by the scanner.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GramKey(pub u64);

impl GramKey {
    /// The raw 64-bit key value.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Byte span in the original scanned content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ByteRange {
    /// Inclusive start byte offset.
    pub start: usize,
    /// Exclusive end byte offset.
    pub end: usize,
}

impl ByteRange {
    /// Build a byte range.
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Convert to a standard range.
    #[must_use]
    pub const fn as_range(self) -> Range<usize> {
        self.start..self.end
    }
}

/// Scan-derived boolean facts about indexed text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ScanFlags(pub u64);

impl ScanFlags {
    const HAS_LF: u64 = 1;
    const HAS_CRLF: u64 = 1 << 1;
    const ENDS_WITH_LF: u64 = 1 << 2;
    const HAS_ASCII_UPPER: u64 = 1 << 3;
    const HAS_ASCII_LOWER: u64 = 1 << 4;
    const HAS_ASCII_DIGIT: u64 = 1 << 5;
    const HAS_ASCII_SPACE: u64 = 1 << 6;
    const HAS_ASCII_WORD: u64 = 1 << 7;
    const HAS_NON_ASCII: u64 = 1 << 8;

    /// Add raw flag bits.
    #[must_use]
    pub const fn with_bits(self, bits: u64) -> Self {
        Self(self.0 | bits)
    }

    /// Compact bit representation.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// True when every bit in `need` is present.
    #[must_use]
    pub const fn contains(self, need: Self) -> bool {
        self.0 & need.0 == need.0
    }

    /// Mark that the content contains `\n`.
    #[must_use]
    pub const fn with_lf(self) -> Self {
        self.with_bits(Self::HAS_LF)
    }

    /// Mark that the content contains a CRLF line ending.
    #[must_use]
    pub const fn with_crlf(self) -> Self {
        self.with_bits(Self::HAS_CRLF)
    }

    /// Mark that the content ends with `\n`.
    #[must_use]
    pub const fn with_ends_with_lf(self) -> Self {
        self.with_bits(Self::ENDS_WITH_LF)
    }

    /// Mark that the content contains ASCII uppercase bytes.
    #[must_use]
    pub const fn with_ascii_upper(self) -> Self {
        self.with_bits(Self::HAS_ASCII_UPPER)
    }

    /// Mark that the content contains ASCII lowercase bytes.
    #[must_use]
    pub const fn with_ascii_lower(self) -> Self {
        self.with_bits(Self::HAS_ASCII_LOWER)
    }

    /// Mark that the content contains ASCII digits.
    #[must_use]
    pub const fn with_ascii_digit(self) -> Self {
        self.with_bits(Self::HAS_ASCII_DIGIT)
    }

    /// Mark that the content contains ASCII whitespace.
    #[must_use]
    pub const fn with_ascii_space(self) -> Self {
        self.with_bits(Self::HAS_ASCII_SPACE)
    }

    /// Mark that the content contains ASCII word bytes.
    #[must_use]
    pub const fn with_ascii_word(self) -> Self {
        self.with_bits(Self::HAS_ASCII_WORD)
    }

    /// Mark that the content contains bytes outside ASCII.
    #[must_use]
    pub const fn with_non_ascii(self) -> Self {
        self.with_bits(Self::HAS_NON_ASCII)
    }

    /// True when the content contains `\n`.
    #[must_use]
    pub const fn has_lf(self) -> bool {
        self.0 & Self::HAS_LF != 0
    }

    /// True when the content contains CRLF.
    #[must_use]
    pub const fn has_crlf(self) -> bool {
        self.0 & Self::HAS_CRLF != 0
    }

    /// True when the content ends with `\n`.
    #[must_use]
    pub const fn ends_with_lf(self) -> bool {
        self.0 & Self::ENDS_WITH_LF != 0
    }

    /// True when the content contains ASCII uppercase bytes.
    #[must_use]
    pub const fn has_ascii_upper(self) -> bool {
        self.0 & Self::HAS_ASCII_UPPER != 0
    }

    /// True when the content contains ASCII lowercase bytes.
    #[must_use]
    pub const fn has_ascii_lower(self) -> bool {
        self.0 & Self::HAS_ASCII_LOWER != 0
    }

    /// True when the content contains ASCII digits.
    #[must_use]
    pub const fn has_ascii_digit(self) -> bool {
        self.0 & Self::HAS_ASCII_DIGIT != 0
    }

    /// True when the content contains ASCII whitespace.
    #[must_use]
    pub const fn has_ascii_space(self) -> bool {
        self.0 & Self::HAS_ASCII_SPACE != 0
    }

    /// True when the content contains ASCII word bytes.
    #[must_use]
    pub const fn has_ascii_word(self) -> bool {
        self.0 & Self::HAS_ASCII_WORD != 0
    }

    /// True when the content contains non-ASCII bytes.
    #[must_use]
    pub const fn has_non_ascii(self) -> bool {
        self.0 & Self::HAS_NON_ASCII != 0
    }
}

/// One sparse n-gram key emitted by `sngram::scan`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScannedGram {
    /// Final index key for this gram.
    ///
    /// Store this value directly. It may include scan-format details such as
    /// virtual document sentinels or case-folded supplements, so re-hashing
    /// `span` bytes from the original content is not equivalent.
    pub key: GramKey,
    /// Related span in the original content, with virtual sentinels removed.
    pub span: ByteRange,
}

impl ScannedGram {
    /// Span in the original content.
    #[must_use]
    pub const fn content_span(&self) -> Range<usize> {
        self.span.as_range()
    }
}

/// Final scan-derived metadata for one indexed text entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScanSummary {
    /// Original content length in bytes.
    pub byte_len: u64,
    /// Number of text lines observed.
    pub line_count: u32,
    /// Number of empty lines observed.
    pub empty_line_count: u32,
    /// Longest line length in bytes, excluding `\n`.
    pub longest_line_len: u32,
    /// Number of gram keys emitted.
    pub gram_count: u32,
    /// Boolean scan facts.
    pub flags: ScanFlags,
    /// Saturating byte histogram.
    pub byte_counts: SaturatingByteCounts256,
    /// First byte seen on each line.
    pub line_start_bytes: ByteSet256,
    /// Last byte seen before each line break or EOF.
    pub line_end_bytes: ByteSet256,
    /// First bytes of the content.
    pub prefix: EdgeBytes,
    /// Last bytes of the content.
    pub suffix: EdgeBytes,
}

/// Event emitted by a scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanEvent<'a> {
    /// One sparse gram key.
    Gram(ScannedGram),
    /// Final per-entry scan summary.
    Finish(&'a ScanSummary),
}

/// Errors from scanning a byte stream.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ScanError {
    /// Reading from the input stream failed.
    #[error("scan input error: {0}")]
    Io(#[from] std::io::Error),
    /// Input was rejected by the scanner's binary-content sniff.
    #[error("scan input appears to be binary")]
    Binary,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_flags_round_trip() {
        let flags = ScanFlags::default()
            .with_lf()
            .with_ascii_upper()
            .with_ascii_lower()
            .with_ends_with_lf();

        assert!(flags.has_lf());
        assert!(!flags.has_crlf());
        assert!(flags.has_ascii_upper());
        assert!(flags.has_ascii_lower());
        assert!(!flags.has_non_ascii());
        assert!(flags.ends_with_lf());
    }

    #[test]
    fn gram_span_is_range() {
        let gram = ScannedGram {
            key: GramKey(7),
            span: ByteRange::new(1, 4),
        };

        assert_eq!(gram.content_span(), 1..4);
    }
}
