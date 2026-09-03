//! Public scan event and metadata types.

use core::ops::Range;

use crate::{ByteSet256, EdgeBytes, SaturatingByteCounts256};

use super::flags::ScanFlags;

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
    fn gram_span_is_range() {
        let gram = ScannedGram {
            key: GramKey(7),
            span: ByteRange::new(1, 4),
        };

        assert_eq!(gram.content_span(), 1..4);
    }
}
