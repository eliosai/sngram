//! Public scan event and metadata types.

use core::ops::Range;

/// Which gram space an emitted gram belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GramSpace {
    /// Raw document bytes.
    Primary,
    /// ASCII-case-folded bytes, hashed in the folded twin space.
    Folded,
}

/// Virtual document boundary information for an emitted gram.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Boundary(u8);

impl Boundary {
    const START: u8 = 1;
    const END: u8 = 1 << 1;

    /// Build boundary metadata from the two possible virtual edges.
    #[must_use]
    pub const fn new(touches_start: bool, touches_end: bool) -> Self {
        Self((touches_start as u8 * Self::START) | (touches_end as u8 * Self::END))
    }

    /// True when the gram includes the virtual leading document sentinel.
    #[must_use]
    pub const fn touches_start(self) -> bool {
        self.0 & Self::START != 0
    }

    /// True when the gram includes the virtual trailing document sentinel.
    #[must_use]
    pub const fn touches_end(self) -> bool {
        self.0 & Self::END != 0
    }

    /// Compact bit representation for storage or foreign-language bindings.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }
}

/// Document facts observed while scanning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ScanFacts(u16);

impl ScanFacts {
    const HAS_LF: u16 = 1;
    const HAS_CRLF: u16 = 1 << 1;
    const HAS_UPPER_ASCII: u16 = 1 << 2;
    const HAS_LOWER_ASCII: u16 = 1 << 3;
    const HAS_NON_ASCII: u16 = 1 << 4;
    const ENDS_WITH_NEWLINE: u16 = 1 << 5;

    /// Mark that the document contains `\n`.
    #[must_use]
    pub const fn with_lf(self) -> Self {
        Self(self.0 | Self::HAS_LF)
    }

    /// Mark that the document contains a CRLF line ending.
    #[must_use]
    pub const fn with_crlf(self) -> Self {
        Self(self.0 | Self::HAS_CRLF)
    }

    /// Mark that the document contains ASCII uppercase bytes.
    #[must_use]
    pub const fn with_upper_ascii(self) -> Self {
        Self(self.0 | Self::HAS_UPPER_ASCII)
    }

    /// Mark that the document contains ASCII lowercase bytes.
    #[must_use]
    pub const fn with_lower_ascii(self) -> Self {
        Self(self.0 | Self::HAS_LOWER_ASCII)
    }

    /// Mark that the document contains bytes outside ASCII.
    #[must_use]
    pub const fn with_non_ascii(self) -> Self {
        Self(self.0 | Self::HAS_NON_ASCII)
    }

    /// Mark that the document's final byte is `\n`.
    #[must_use]
    pub const fn with_ends_with_newline(self) -> Self {
        Self(self.0 | Self::ENDS_WITH_NEWLINE)
    }

    /// True when the document contains `\n`.
    #[must_use]
    pub const fn has_lf(self) -> bool {
        self.0 & Self::HAS_LF != 0
    }

    /// True when the document contains a CRLF line ending.
    #[must_use]
    pub const fn has_crlf(self) -> bool {
        self.0 & Self::HAS_CRLF != 0
    }

    /// True when the document contains ASCII uppercase bytes.
    #[must_use]
    pub const fn has_upper_ascii(self) -> bool {
        self.0 & Self::HAS_UPPER_ASCII != 0
    }

    /// True when the document contains ASCII lowercase bytes.
    #[must_use]
    pub const fn has_lower_ascii(self) -> bool {
        self.0 & Self::HAS_LOWER_ASCII != 0
    }

    /// True when the document contains bytes outside ASCII.
    #[must_use]
    pub const fn has_non_ascii(self) -> bool {
        self.0 & Self::HAS_NON_ASCII != 0
    }

    /// True when the document's final byte is `\n`.
    #[must_use]
    pub const fn ends_with_newline(self) -> bool {
        self.0 & Self::ENDS_WITH_NEWLINE != 0
    }

    /// Compact bit representation for storage or foreign-language bindings.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }
}

/// One sparse n-gram emitted by [`sngram::scan`](https://docs.rs/sngram/latest/sngram/fn.scan.html).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScannedGram<'a> {
    /// Gram bytes in the emitted gram space.
    pub bytes: &'a [u8],
    /// Hash value for this gram in its gram space.
    pub hash: u64,
    /// The gram space this key belongs to.
    pub space: GramSpace,
    /// Start offset in the scanned stream.
    pub scanned_start: usize,
    /// End offset in the scanned stream.
    pub scanned_end: usize,
    /// Start offset clamped into the original document content.
    pub content_start: usize,
    /// End offset clamped into the original document content.
    pub content_end: usize,
    /// Virtual boundary bytes included by this gram, if any.
    pub boundary: Boundary,
}

impl ScannedGram<'_> {
    /// Span in the scanned stream.
    #[must_use]
    pub const fn scanned_span(&self) -> Range<usize> {
        self.scanned_start..self.scanned_end
    }

    /// Span in the original content, with virtual sentinels removed.
    #[must_use]
    pub const fn content_span(&self) -> Range<usize> {
        self.content_start..self.content_end
    }
}

/// Final metadata for one completed scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScanSummary {
    /// Original document bytes read from the input stream.
    pub content_bytes: usize,
    /// Bytes in the scanned stream, including virtual document sentinels.
    pub scanned_bytes: usize,
    /// Number of primary-space grams emitted.
    pub primary_grams: usize,
    /// Number of folded-space grams emitted.
    pub folded_grams: usize,
    /// Document facts observed during scanning.
    pub facts: ScanFacts,
}

impl ScanSummary {
    /// Total number of emitted grams.
    #[must_use]
    pub const fn grams(self) -> usize {
        self.primary_grams + self.folded_grams
    }
}

/// Event emitted by a scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanEvent<'a> {
    /// One sparse gram.
    Gram(ScannedGram<'a>),
    /// Final per-document summary.
    Finish(ScanSummary),
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
    fn boundary_bits_round_trip() {
        let none = Boundary::new(false, false);
        let both = Boundary::new(true, true);

        assert_eq!(none.bits(), 0);
        assert!(!none.touches_start());
        assert!(!none.touches_end());
        assert!(both.touches_start());
        assert!(both.touches_end());
        assert_eq!(both.bits(), 3);
    }

    #[test]
    fn facts_bits_round_trip() {
        let facts = ScanFacts::default()
            .with_lf()
            .with_upper_ascii()
            .with_lower_ascii()
            .with_ends_with_newline();

        assert!(facts.has_lf());
        assert!(!facts.has_crlf());
        assert!(facts.has_upper_ascii());
        assert!(facts.has_lower_ascii());
        assert!(!facts.has_non_ascii());
        assert!(facts.ends_with_newline());
    }

    #[test]
    fn gram_spans_are_ranges() {
        let gram = ScannedGram {
            bytes: b"abc",
            hash: 7,
            space: GramSpace::Primary,
            scanned_start: 1,
            scanned_end: 4,
            content_start: 0,
            content_end: 3,
            boundary: Boundary::new(false, false),
        };

        assert_eq!(gram.scanned_span(), 1..4);
        assert_eq!(gram.content_span(), 0..3);
    }

    #[test]
    fn summary_counts_all_grams() {
        let summary = ScanSummary {
            content_bytes: 10,
            scanned_bytes: 12,
            primary_grams: 3,
            folded_grams: 4,
            facts: ScanFacts::default(),
        };

        assert_eq!(summary.grams(), 7);
    }
}
