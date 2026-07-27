/// Scan-derived boolean facts about indexed text
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

    /// Add raw flag bits
    #[must_use]
    pub const fn with_bits(self, bits: u64) -> Self {
        Self(self.0 | bits)
    }

    /// Compact bit representation
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// True when every bit in `need` is present
    #[must_use]
    pub const fn contains(self, need: Self) -> bool {
        self.0 & need.0 == need.0
    }

    /// Mark that the content contains `\n`
    #[must_use]
    pub const fn with_lf(self) -> Self {
        self.with_bits(Self::HAS_LF)
    }

    /// Mark that the content contains a CRLF line ending
    #[must_use]
    pub const fn with_crlf(self) -> Self {
        self.with_bits(Self::HAS_CRLF)
    }

    /// Mark that the content ends with `\n`
    #[must_use]
    pub const fn with_ends_with_lf(self) -> Self {
        self.with_bits(Self::ENDS_WITH_LF)
    }

    /// Mark that the content contains ASCII uppercase bytes
    #[must_use]
    pub const fn with_ascii_upper(self) -> Self {
        self.with_bits(Self::HAS_ASCII_UPPER)
    }

    /// Mark that the content contains ASCII lowercase bytes
    #[must_use]
    pub const fn with_ascii_lower(self) -> Self {
        self.with_bits(Self::HAS_ASCII_LOWER)
    }

    /// Mark that the content contains ASCII digits
    #[must_use]
    pub const fn with_ascii_digit(self) -> Self {
        self.with_bits(Self::HAS_ASCII_DIGIT)
    }

    /// Mark that the content contains ASCII whitespace
    #[must_use]
    pub const fn with_ascii_space(self) -> Self {
        self.with_bits(Self::HAS_ASCII_SPACE)
    }

    /// Mark that the content contains ASCII word bytes
    #[must_use]
    pub const fn with_ascii_word(self) -> Self {
        self.with_bits(Self::HAS_ASCII_WORD)
    }

    /// Mark that the content contains bytes outside ASCII
    #[must_use]
    pub const fn with_non_ascii(self) -> Self {
        self.with_bits(Self::HAS_NON_ASCII)
    }

    /// True when the content contains `\n`
    #[must_use]
    pub const fn has_lf(self) -> bool {
        self.0 & Self::HAS_LF != 0
    }

    /// True when the content contains CRLF
    #[must_use]
    pub const fn has_crlf(self) -> bool {
        self.0 & Self::HAS_CRLF != 0
    }

    /// True when the content ends with `\n`
    #[must_use]
    pub const fn ends_with_lf(self) -> bool {
        self.0 & Self::ENDS_WITH_LF != 0
    }

    /// True when the content contains ASCII uppercase bytes
    #[must_use]
    pub const fn has_ascii_upper(self) -> bool {
        self.0 & Self::HAS_ASCII_UPPER != 0
    }

    /// True when the content contains ASCII lowercase bytes
    #[must_use]
    pub const fn has_ascii_lower(self) -> bool {
        self.0 & Self::HAS_ASCII_LOWER != 0
    }

    /// True when the content contains ASCII digits
    #[must_use]
    pub const fn has_ascii_digit(self) -> bool {
        self.0 & Self::HAS_ASCII_DIGIT != 0
    }

    /// True when the content contains ASCII whitespace
    #[must_use]
    pub const fn has_ascii_space(self) -> bool {
        self.0 & Self::HAS_ASCII_SPACE != 0
    }

    /// True when the content contains ASCII word bytes
    #[must_use]
    pub const fn has_ascii_word(self) -> bool {
        self.0 & Self::HAS_ASCII_WORD != 0
    }

    /// True when the content contains non-ASCII bytes
    #[must_use]
    pub const fn has_non_ascii(self) -> bool {
        self.0 & Self::HAS_NON_ASCII != 0
    }
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
}
