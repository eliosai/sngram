//! Shared file classification for indexing.
//!
//! Binary files are excluded from the full-corpus output modes. Grams are still
//! indexed over the bytes binary detection lets the search path report a match
//! in, so a candidate is never missed. Oversized, encoded, high-entropy, and
//! scanner-rejected files are not indexed for their grams; they are recorded
//! as forced candidates so the verifier still searches them with the
//! configured text semantics, keeping the index sound and small.

use std::{
    fs::File,
    io::{self, Read},
    path::Path,
};

use crate::nulquit::has_decoding_bom;

/// Files at or above this size are skipped to avoid the scanner's 4 GiB limit.
pub const MAX_INDEXABLE_LEN: u64 = 4 * 1024 * 1024 * 1024;

/// Bytes read per chunk when sniffing a path for binary content.
const BINARY_SCAN_BYTES: usize = 8 * 1024;

/// Smallest file the entropy guard applies to.
const ENTROPY_MIN_BYTES: usize = 4 * 1024;

/// Return true when the file is too large for the scanner and must be skipped.
pub const fn is_oversized(len: u64) -> bool {
    len >= MAX_INDEXABLE_LEN
}

/// The leading bytes a match can still be reported in.
///
/// Binary detection quits at the first NUL, so a match after it is never
/// reported. Indexing this prefix keeps the gram index a superset of what the
/// search path can find, even in files the output modes treat as binary.
pub fn searchable_prefix(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|&byte| byte == 0) {
        Some(nul) => &bytes[..nul],
        None => bytes,
    }
}

/// Return true when the search path treats the file at `path` as binary.
///
/// This drives the full-corpus output modes, which never name a binary file.
/// It mirrors the searcher exactly: quit detection flags a file on its first
/// NUL, and streams opening with a decoding BOM are transcoded instead. It
/// does not decide gram coverage: `searchable_prefix` does that, so a match
/// before the first NUL still resolves through the index.
pub fn is_binary_path(path: &Path) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let mut buffer = [0u8; BINARY_SCAN_BYTES];
    let mut first = true;
    loop {
        let len = file.read(&mut buffer)?;
        if len == 0 {
            return Ok(false);
        }
        let bytes = &buffer[..len];
        if first {
            if has_decoding_bom(bytes) {
                return Ok(false);
            }
            first = false;
        }
        if bytes.contains(&0) {
            return Ok(true);
        }
    }
}

/// Return true when unique grams per byte exceed the high-entropy cap.
///
/// Sparse scanning can emit more than one unique gram per byte on diverse
/// but legitimate source files, so this guard sits above normal source/docs
/// density and targets random printable/base64-like blobs that approach two
/// unique grams per byte.
pub const fn is_high_entropy(len: usize, unique: usize) -> bool {
    len >= ENTROPY_MIN_BYTES && unique.saturating_mul(2) > len.saturating_mul(3)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        BINARY_SCAN_BYTES, is_binary_path, is_high_entropy, is_oversized, searchable_prefix,
    };

    fn scratch(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("eg-classify-{name}-"))
            .tempdir()
            .expect("scratch dir")
    }

    fn classify_bytes(name: &str, bytes: &[u8]) -> bool {
        let dir = scratch(name);
        let path = dir.path().join("fixture.bin");
        fs::write(&path, bytes).expect("write fixture");
        is_binary_path(&path).expect("classify path")
    }

    #[test]
    fn oversize_boundary() {
        assert!(!is_oversized(0));
        assert!(!is_oversized((4 * 1024 * 1024 * 1024) - 1));
        assert!(is_oversized(4 * 1024 * 1024 * 1024));
    }

    #[test]
    fn binary_detects_any_nul() {
        assert!(!classify_bytes("plain", b"plain ascii text"));
        assert!(classify_bytes("nul", b"abc\0def"));
    }

    #[test]
    fn binary_follows_searcher_semantics_not_signatures() {
        assert!(
            !classify_bytes("parquet", b"PAR1abcdefgh"),
            "a NUL-free signature blob is text to the searcher, so output modes must name it"
        );
        assert!(classify_bytes("spng", b"SPNG\x01\x00\x00\x00abcdefgh"));
    }

    #[test]
    fn searchable_prefix_stops_at_the_first_nul() {
        assert_eq!(b"abc", searchable_prefix(b"abc\0def"));
        assert_eq!(b"no nul", searchable_prefix(b"no nul"));
        assert_eq!(b"", searchable_prefix(b"\0leading"));
    }

    #[test]
    fn searchable_prefix_keeps_text_a_binary_file_can_still_match_in() {
        let mut late = b"needle\n".to_vec();
        late.extend(std::iter::repeat_n(b'a', 4 * 1024));
        late.push(0);
        late.extend_from_slice(b"tail");

        assert!(
            classify_bytes("late", &late),
            "the file is binary for the output modes"
        );
        assert!(
            searchable_prefix(&late).starts_with(b"needle"),
            "yet the index must still cover the text before the NUL"
        );
    }

    #[test]
    fn entropy_guard_ignores_small_and_repetitive() {
        assert!(!is_high_entropy(10, 8));
        assert!(!is_high_entropy(8192, 100));
        assert!(
            !is_high_entropy(8192, 7500),
            "dense legit code stays indexed"
        );
        assert!(!is_high_entropy(8192, 12_288));
        assert!(
            is_high_entropy(8192, 15_000),
            "near two unique grams per byte is random printable data"
        );
    }

    #[test]
    fn bom_text_is_not_binary() {
        assert!(
            !classify_bytes("bom", &[0xFF, 0xFE, b'a', 0x00]),
            "BOM text is handled as an encoded forced candidate"
        );
    }

    #[test]
    fn path_binary_detection_scans_past_the_first_chunk() {
        let dir = scratch("late-nul");
        let path = dir.path().join("late-nul.bin");
        let mut bytes = vec![b'a'; BINARY_SCAN_BYTES + 7];
        bytes.push(0);
        fs::write(&path, bytes).expect("write fixture");

        assert!(is_binary_path(&path).expect("classify path"));
    }

    #[test]
    fn path_bom_text_is_not_reclassified_by_utf16_nuls() {
        let dir = scratch("utf16");
        let path = dir.path().join("utf16.txt");
        fs::write(&path, [0xFF, 0xFE, b'a', 0x00, b'b', 0x00]).expect("write fixture");

        assert!(!is_binary_path(&path).expect("classify path"));
    }
}
