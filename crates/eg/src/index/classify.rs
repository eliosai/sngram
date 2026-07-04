//! Shared file classification for indexing.
//!
//! Binary files are excluded from indexed search. Oversized, encoded, and
//! high-entropy non-binary files are not indexed for their grams; they are
//! recorded as forced candidates so the verifier still searches them with the
//! configured text semantics, keeping the index sound and small.

use std::{
    fs::File,
    io::{self, Read},
    path::Path,
};

use sngram_types::Content;

/// Files at or above this size are skipped to avoid the scanner's 4 GiB limit.
pub(super) const MAX_INDEXABLE_LEN: u64 = 4 * 1024 * 1024 * 1024;

/// Bytes read per chunk when sniffing a path for binary content.
const BINARY_SCAN_BYTES: usize = 8 * 1024;

/// Smallest file the entropy guard applies to.
const ENTROPY_MIN_BYTES: usize = 4 * 1024;

/// Return true when the file is too large for the scanner and must be skipped.
pub(super) const fn is_oversized(len: u64) -> bool {
    len >= MAX_INDEXABLE_LEN
}

/// Return true when the bytes look like binary data.
///
/// Indexed search keeps the policy stricter than ripgrep's early-window rule:
/// any NUL in the indexed byte stream excludes the file from the sparse index.
/// BOM-encoded text is handled separately as a forced candidate, so UTF-16/32
/// NUL bytes do not cause those files to be dropped.
pub(super) fn is_binary(bytes: &[u8]) -> bool {
    if has_decoding_bom(bytes) {
        return false;
    }
    bytes.contains(&0) || has_binary_head(bytes)
}

/// Return true when a file at `path` looks binary without loading it all.
pub(super) fn is_binary_path(path: &Path) -> io::Result<bool> {
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
            if has_binary_head(bytes) {
                return Ok(true);
            }
            first = false;
        }
        if bytes.contains(&0) {
            return Ok(true);
        }
    }
}

fn has_binary_head(bytes: &[u8]) -> bool {
    Content::new(bytes).has_binary_signature() || Content::new(bytes).is_likely_binary()
}

/// Return true when unique grams per byte exceed the high-entropy cap.
///
/// Sparse scanning can emit more than one unique gram per byte on diverse
/// but legitimate source files, so this guard sits above normal source/docs
/// density and targets random printable/base64-like blobs that approach two
/// unique grams per byte.
pub(super) const fn is_high_entropy(len: usize, unique: usize) -> bool {
    len >= ENTROPY_MIN_BYTES && unique.saturating_mul(2) > len.saturating_mul(3)
}

/// Return true when the file starts with a UTF-16/UTF-32 byte-order mark.
pub(super) fn has_decoding_bom(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xFE])
        || bytes.starts_with(&[0xFE, 0xFF])
        || bytes.starts_with(&[0xFF, 0xFE, 0x00, 0x00])
        || bytes.starts_with(&[0x00, 0x00, 0xFE, 0xFF])
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        BINARY_SCAN_BYTES, has_decoding_bom, is_binary, is_binary_path, is_high_entropy,
        is_oversized,
    };

    fn scratch_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("eg-classify-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&root).expect("scratch dir");
        root.join(name)
    }

    #[test]
    fn oversize_boundary() {
        assert!(!is_oversized(0));
        assert!(!is_oversized((4 * 1024 * 1024 * 1024) - 1));
        assert!(is_oversized(4 * 1024 * 1024 * 1024));
    }

    #[test]
    fn binary_detects_any_nul() {
        assert!(!is_binary(b"plain ascii text"));
        assert!(is_binary(b"abc\0def"));
        let mut late = vec![b'a'; 16 * 1024];
        late.push(0);
        assert!(is_binary(&late));
    }

    #[test]
    fn binary_detects_known_signatures() {
        assert!(is_binary(b"PAR1abcdefgh"));
        assert!(is_binary(b"SPNG\x01\x00\x00\x00abcdefgh"));
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
    fn bom_prefixes() {
        assert!(has_decoding_bom(&[0xFF, 0xFE, b'a']));
        assert!(has_decoding_bom(&[0xFE, 0xFF, b'a']));
        assert!(!has_decoding_bom(b"no bom"));
        assert!(
            !is_binary(&[0xFF, 0xFE, b'a', 0x00]),
            "BOM text is handled as an encoded forced candidate"
        );
    }

    #[test]
    fn path_binary_detection_scans_past_the_first_chunk() {
        let path = scratch_path("late-nul.bin");
        let mut bytes = vec![b'a'; BINARY_SCAN_BYTES + 7];
        bytes.push(0);
        fs::write(&path, bytes).expect("write fixture");

        assert!(is_binary_path(&path).expect("classify path"));

        fs::remove_file(&path).expect("remove fixture");
        fs::remove_dir(path.parent().expect("parent")).expect("remove scratch dir");
    }

    #[test]
    fn path_bom_text_is_not_reclassified_by_utf16_nuls() {
        let path = scratch_path("utf16.txt");
        fs::write(&path, [0xFF, 0xFE, b'a', 0x00, b'b', 0x00]).expect("write fixture");

        assert!(!is_binary_path(&path).expect("classify path"));

        fs::remove_file(&path).expect("remove fixture");
        fs::remove_dir(path.parent().expect("parent")).expect("remove scratch dir");
    }
}
