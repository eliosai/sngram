//! Content wrapper with binary detection utilities.

/// Zero-cost wrapper around byte content.
#[derive(Debug, Clone, Copy)]
pub struct Content<'a>(&'a [u8]);

const BINARY_SIGS: &[&[u8]] = &[
    b"\x7fELF", b"MZ",
    b"\xfe\xed\xfa\xce", b"\xfe\xed\xfa\xcf",
    b"\xce\xfa\xed\xfe", b"\xcf\xfa\xed\xfe",
    b"\xca\xfe\xba\xbe",
    b"\x89PNG", b"\xff\xd8\xff", b"GIF8", b"BM",
    b"\x00\x00\x01\x00", b"RIFF",
    b"\x49\x49\x2a\x00", b"\x4d\x4d\x00\x2a",
    b"PK\x03\x04", b"\x1f\x8b", b"BZh", b"\xfd7zXZ",
    b"7z\xbc\xaf\x27\x1c", b"\x52\x61\x72\x21",
    b"\x28\xb5\x2f\xfd", b"\x04\x22\x4d\x18",
    b"%PDF", b"\xd0\xcf\x11\xe0",
    b"\x1a\x45\xdf\xa3",
    b"\x00\x00\x00\x1c\x66\x74\x79\x70",
    b"fLaC", b"OggS", b"ID3", b"\xff\xfb", b"\xff\xf3",
    b"SQLite format 3", b"\x00asm",
    b"PAR1", b"ORC", b"ARROW1",
];

impl<'a> Content<'a> {
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Self { Self(bytes) }

    #[must_use]
    pub const fn as_bytes(&self) -> &'a [u8] { self.0 }

    #[must_use]
    pub const fn len(&self) -> usize { self.0.len() }

    #[must_use]
    pub const fn is_empty(&self) -> bool { self.0.is_empty() }

    #[must_use]
    pub const fn exceeds(&self, max_bytes: usize) -> bool { self.0.len() > max_bytes }

    #[must_use]
    pub fn has_binary_signature(&self) -> bool {
        BINARY_SIGS.iter().any(|sig| self.0.starts_with(sig))
    }

    #[allow(clippy::indexing_slicing, reason = "sample bounds checked by min")]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_elf() {
        let c = Content::new(b"\x7fELF\x00\x00\x00rest");
        assert!(c.has_binary_signature());
    }

    #[test]
    fn detects_png() {
        let c = Content::new(b"\x89PNG\r\n\x1a\ndata");
        assert!(c.has_binary_signature());
    }

    #[test]
    fn text_has_no_binary_signature() {
        let c = Content::new(b"fn main() { println!(\"hi\"); }");
        assert!(!c.has_binary_signature());
    }

    #[test]
    fn nulls_detected_as_binary() {
        let mut data = vec![0u8; 100];
        data[50] = b'a';
        let c = Content::new(&data);
        assert!(c.is_likely_binary());
    }

    #[test]
    fn source_code_not_binary() {
        let c = Content::new(b"fn main() {\n    let x = 42;\n}\n");
        assert!(!c.is_likely_binary());
    }

    #[test]
    fn short_content_not_binary() {
        let c = Content::new(b"\x00\x00\x00");
        assert!(!c.is_likely_binary());
    }

    #[test]
    fn exceeds_reports_correctly() {
        let c = Content::new(&[0; 1000]);
        assert!(c.exceeds(999));
        assert!(!c.exceeds(1000));
    }
}
