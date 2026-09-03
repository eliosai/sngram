//! Scan-derived boolean facts about indexed text

/// Scan-derived boolean facts about indexed text
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ScanFlags(u64);

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

    /// Flags from their compact bit representation
    #[must_use]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// The compact bit representation
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Mark that the content contains `\n`
    #[must_use]
    pub const fn with_lf(self) -> Self {
        self.with(Self::HAS_LF)
    }

    /// Mark that the content contains a CRLF line ending
    #[must_use]
    pub const fn with_crlf(self) -> Self {
        self.with(Self::HAS_CRLF)
    }

    /// Mark that the content ends with `\n`
    #[must_use]
    pub const fn with_ends_with_lf(self) -> Self {
        self.with(Self::ENDS_WITH_LF)
    }

    /// Mark that the content contains ASCII uppercase bytes
    #[must_use]
    pub const fn with_ascii_upper(self) -> Self {
        self.with(Self::HAS_ASCII_UPPER)
    }

    /// Mark that the content contains ASCII lowercase bytes
    #[must_use]
    pub const fn with_ascii_lower(self) -> Self {
        self.with(Self::HAS_ASCII_LOWER)
    }

    /// Mark that the content contains ASCII digits
    #[must_use]
    pub const fn with_ascii_digit(self) -> Self {
        self.with(Self::HAS_ASCII_DIGIT)
    }

    /// Mark that the content contains ASCII whitespace
    #[must_use]
    pub const fn with_ascii_space(self) -> Self {
        self.with(Self::HAS_ASCII_SPACE)
    }

    /// Mark that the content contains ASCII word bytes
    #[must_use]
    pub const fn with_ascii_word(self) -> Self {
        self.with(Self::HAS_ASCII_WORD)
    }

    /// Mark that the content contains bytes outside ASCII
    #[must_use]
    pub const fn with_non_ascii(self) -> Self {
        self.with(Self::HAS_NON_ASCII)
    }

    /// True when the content contains `\n`
    #[must_use]
    pub const fn has_lf(self) -> bool {
        self.has(Self::HAS_LF)
    }

    /// True when the content contains CRLF
    #[must_use]
    pub const fn has_crlf(self) -> bool {
        self.has(Self::HAS_CRLF)
    }

    /// True when the content ends with `\n`
    #[must_use]
    pub const fn ends_with_lf(self) -> bool {
        self.has(Self::ENDS_WITH_LF)
    }

    /// True when the content contains ASCII uppercase bytes
    #[must_use]
    pub const fn has_ascii_upper(self) -> bool {
        self.has(Self::HAS_ASCII_UPPER)
    }

    /// True when the content contains ASCII lowercase bytes
    #[must_use]
    pub const fn has_ascii_lower(self) -> bool {
        self.has(Self::HAS_ASCII_LOWER)
    }

    /// True when the content contains ASCII digits
    #[must_use]
    pub const fn has_ascii_digit(self) -> bool {
        self.has(Self::HAS_ASCII_DIGIT)
    }

    /// True when the content contains ASCII whitespace
    #[must_use]
    pub const fn has_ascii_space(self) -> bool {
        self.has(Self::HAS_ASCII_SPACE)
    }

    /// True when the content contains ASCII word bytes
    #[must_use]
    pub const fn has_ascii_word(self) -> bool {
        self.has(Self::HAS_ASCII_WORD)
    }

    /// True when the content contains non-ASCII bytes
    #[must_use]
    pub const fn has_non_ascii(self) -> bool {
        self.has(Self::HAS_NON_ASCII)
    }

    const fn with(self, bit: u64) -> Self {
        Self(self.0 | bit)
    }

    const fn has(self, bit: u64) -> bool {
        self.0 & bit != 0
    }
}

#[cfg(test)]
mod tests {
    use super::ScanFlags;

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
        assert_eq!(ScanFlags::from_bits(flags.bits()), flags);
    }
}
