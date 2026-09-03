//! Verbatim copy of the baseline binary sniff

/// Zero-cost borrowed wrapper around byte content.
#[derive(Debug, Clone, Copy)]
pub struct Content<'a>(&'a [u8]);

// conservative binary signatures checked before the control-byte sniff
const BINARY_SIGS: &[&[u8]] = &[
    b"SPNG",
    b"\x7fELF",
    b"MZ",
    b"\xfe\xed\xfa\xce",
    b"\xfe\xed\xfa\xcf",
    b"\xce\xfa\xed\xfe",
    b"\xcf\xfa\xed\xfe",
    b"\xca\xfe\xba\xbe",
    b"\x89PNG",
    b"\xff\xd8\xff",
    b"GIF8",
    b"BM",
    b"\x00\x00\x01\x00",
    b"RIFF",
    b"\x49\x49\x2a\x00",
    b"\x4d\x4d\x00\x2a",
    b"PK\x03\x04",
    b"\x1f\x8b",
    b"BZh",
    b"\xfd7zXZ",
    b"7z\xbc\xaf\x27\x1c",
    b"\x52\x61\x72\x21",
    b"\x28\xb5\x2f\xfd",
    b"\x04\x22\x4d\x18",
    b"%PDF",
    b"\xd0\xcf\x11\xe0",
    b"\x1a\x45\xdf\xa3",
    b"\x00\x00\x00\x1c\x66\x74\x79\x70",
    b"fLaC",
    b"OggS",
    b"ID3",
    b"\xff\xfb",
    b"\xff\xf3",
    b"SQLite format 3",
    b"\x00asm",
    b"PAR1",
    b"ORC",
    b"ARROW1",
];

impl<'a> Content<'a> {
    /// Wrap borrowed bytes as content.
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    /// Whether the content starts with a known binary file signature.
    #[must_use]
    pub fn has_binary_signature(&self) -> bool {
        BINARY_SIGS.iter().any(|sig| self.0.starts_with(sig))
    }

    /// Control-byte sniff over the head: NULs or dense non-text bytes.
    #[must_use]
    pub fn is_likely_binary(&self) -> bool {
        let end = self.0.len().min(8192);
        let sample = &self.0[..end];
        let non_text = sample
            .iter()
            .filter(|&&b| b == 0 || (b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t'))
            .count();
        sample.len() >= 10 && non_text > sample.len() / 10
    }
}
