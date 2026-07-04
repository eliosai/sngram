//! Weight table type.

use crate::error::TableError;

const HEADER_SIZE: usize = 16;
const WEIGHTS_COUNT: usize = 65_536;
const MAGIC: &[u8; 4] = b"SPNG";

/// Total v1 table binary size; v2 adds a provenance block after the weights.
pub const TABLE_BINARY_SIZE: usize = HEADER_SIZE + WEIGHTS_COUNT * 4;

/// Longest accepted provenance block, in bytes.
pub const PROVENANCE_MAX: usize = 1024;

/// 256x256 character-pair weight table.
#[derive(Debug, Clone)]
pub struct WeightTable {
    weights: Box<[u32; WEIGHTS_COUNT]>,
    version: u32,
    provenance: Option<String>,
}

impl WeightTable {
    /// # Errors
    ///
    /// Returns `TableError` on malformed data, an unknown version, or a
    /// checksum mismatch.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, TableError> {
        if bytes.len() < HEADER_SIZE {
            return Err(TableError::Truncated(bytes.len()));
        }
        if bytes.get(..4) != Some(MAGIC.as_slice()) {
            return Err(TableError::InvalidMagic);
        }
        let version = read_u32_le(bytes, 4)?;
        let expected_crc = read_u32_le(bytes, 8)?;
        let body = bytes
            .get(HEADER_SIZE..)
            .ok_or(TableError::Truncated(bytes.len()))?;
        let provenance = version_provenance(version, bytes.len(), body)?;
        verify_checksum(expected_crc, body)?;
        let data = body
            .get(..WEIGHTS_COUNT * 4)
            .ok_or(TableError::Truncated(bytes.len()))?;
        Ok(Self {
            weights: parse_weights(data),
            version,
            provenance,
        })
    }

    /// Format version the table was minted with.
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// Provenance record minted with the table; v1 tables carry none.
    #[must_use]
    pub fn provenance(&self) -> Option<&str> {
        self.provenance.as_deref()
    }

    /// The weight of one byte pair.
    #[allow(clippy::indexing_slicing, reason = "u8<<8|u8 <= 65535 < 65536")]
    #[must_use]
    pub fn weight(&self, c1: u8, c2: u8) -> u32 {
        self.weights[usize::from(c1) << 8 | usize::from(c2)]
    }

    /// Full 256x256 weight matrix as a fixed-size array reference.
    ///
    /// Indexing with `(c1 << 8) | c2` (max 65535) is provably in-bounds for a
    /// `&[u32; 65536]`, so the optimizer drops the bounds check that a slice
    /// index would keep — this is the hot-loop accessor.
    #[must_use]
    pub fn matrix(&self) -> &[u32; WEIGHTS_COUNT] {
        &self.weights
    }
}

/// The body must hash to the header's stored checksum.
fn verify_checksum(expected: u32, body: &[u8]) -> Result<(), TableError> {
    let actual = crc32fast::hash(body);
    if expected == actual {
        Ok(())
    } else {
        Err(TableError::Checksum { expected, actual })
    }
}

/// Version gate: v1 is the exact legacy size, v2 carries a provenance tail.
fn version_provenance(
    version: u32,
    total_len: usize,
    body: &[u8],
) -> Result<Option<String>, TableError> {
    match version {
        1 => {
            if total_len != TABLE_BINARY_SIZE {
                return Err(TableError::InvalidSize(total_len));
            }
            Ok(None)
        },
        2 => Ok(Some(parse_provenance(body)?)),
        other => Err(TableError::InvalidVersion(other)),
    }
}

/// The v2 body is the weights, a u16 LE length, then that many UTF-8 bytes.
fn parse_provenance(body: &[u8]) -> Result<String, TableError> {
    let tail = body
        .get(WEIGHTS_COUNT * 4..)
        .ok_or(TableError::Truncated(body.len()))?;
    let len_bytes: [u8; 2] = tail
        .get(..2)
        .and_then(|s| s.try_into().ok())
        .ok_or(TableError::Truncated(tail.len()))?;
    let len = usize::from(u16::from_le_bytes(len_bytes));
    if len > PROVENANCE_MAX {
        return Err(TableError::InvalidProvenance);
    }
    let text = tail.get(2..2 + len).ok_or(TableError::InvalidProvenance)?;
    if tail.len() != 2 + len {
        return Err(TableError::InvalidProvenance);
    }
    String::from_utf8(text.to_vec()).map_err(|_| TableError::InvalidProvenance)
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32, TableError> {
    bytes
        .get(offset..offset + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(TableError::Truncated(bytes.len()))
}

#[allow(clippy::indexing_slicing, reason = "data.len() == WEIGHTS_COUNT * 4")]
#[allow(
    clippy::expect_used,
    reason = "the vec has exactly WEIGHTS_COUNT elements; conversion cannot fail"
)]
fn parse_weights(data: &[u8]) -> Box<[u32; WEIGHTS_COUNT]> {
    let mut weights: Box<[u32; WEIGHTS_COUNT]> = vec![0u32; WEIGHTS_COUNT]
        .into_boxed_slice()
        .try_into()
        .expect("WEIGHTS_COUNT elements");
    for (i, w) in weights.iter_mut().enumerate() {
        let off = i * 4;
        *w = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
    }
    weights
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_table_bytes() -> Vec<u8> {
        let mut buf = vec![0u8; TABLE_BINARY_SIZE];
        buf[..4].copy_from_slice(b"SPNG");
        buf[4..8].copy_from_slice(&1u32.to_le_bytes());

        for c1 in 0u8..=255 {
            for c2 in 0u8..=255 {
                let w = crc32fast::hash(&[c1, c2]);
                let idx = usize::from(c1) << 8 | usize::from(c2);
                let off = 16 + idx * 4;
                buf[off..off + 4].copy_from_slice(&w.to_le_bytes());
            }
        }

        let crc = crc32fast::hash(&buf[16..]);
        buf[8..12].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    fn test_table_bytes_v2(provenance: &str) -> Vec<u8> {
        let mut buf = test_table_bytes();
        buf[4..8].copy_from_slice(&2u32.to_le_bytes());
        let len = u16::try_from(provenance.len()).unwrap();
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(provenance.as_bytes());
        let crc = crc32fast::hash(&buf[16..]);
        buf[8..12].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    #[test]
    fn loads_valid_table() {
        let bytes = test_table_bytes();
        let table = WeightTable::from_bytes(&bytes).unwrap();
        assert_eq!(table.version(), 1);
        assert_eq!(table.provenance(), None);
        assert_ne!(table.weight(b'a', b'b'), 0);
    }

    #[test]
    fn loads_v2_with_provenance() {
        let bytes = test_table_bytes_v2("corpus=fs-validate;date=2026-07-03;commit=abc123");
        let table = WeightTable::from_bytes(&bytes).unwrap();
        assert_eq!(table.version(), 2);
        assert_eq!(
            table.provenance(),
            Some("corpus=fs-validate;date=2026-07-03;commit=abc123")
        );
        assert_ne!(table.weight(b'a', b'b'), 0);
    }

    #[test]
    fn v2_weights_match_v1() {
        let v1 = WeightTable::from_bytes(&test_table_bytes()).unwrap();
        let v2 = WeightTable::from_bytes(&test_table_bytes_v2("p")).unwrap();
        assert_eq!(v1.weight(b'f', b'n'), v2.weight(b'f', b'n'));
        assert_eq!(v1.weight(0, 255), v2.weight(0, 255));
    }

    #[test]
    fn rejects_unknown_version() {
        let mut bytes = test_table_bytes();
        bytes[4..8].copy_from_slice(&3u32.to_le_bytes());
        let crc = crc32fast::hash(&bytes[16..]);
        bytes[8..12].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            WeightTable::from_bytes(&bytes),
            Err(TableError::InvalidVersion(3))
        ));
    }

    #[test]
    fn rejects_v2_at_v1_size_as_truncated_provenance() {
        let mut bytes = test_table_bytes();
        bytes[4..8].copy_from_slice(&2u32.to_le_bytes());
        let crc = crc32fast::hash(&bytes[16..]);
        bytes[8..12].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            WeightTable::from_bytes(&bytes),
            Err(TableError::Truncated(_))
        ));
    }

    #[test]
    fn rejects_oversized_provenance() {
        let big = "x".repeat(PROVENANCE_MAX + 1);
        let bytes = test_table_bytes_v2(&big);
        assert!(matches!(
            WeightTable::from_bytes(&bytes),
            Err(TableError::InvalidProvenance)
        ));
    }

    #[test]
    fn rejects_non_utf8_provenance() {
        let mut bytes = test_table_bytes_v2("ok");
        let at = bytes.len() - 1;
        bytes[at] = 0xFF;
        let crc = crc32fast::hash(&bytes[16..]);
        bytes[8..12].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            WeightTable::from_bytes(&bytes),
            Err(TableError::InvalidProvenance)
        ));
    }

    #[test]
    fn rejects_trailing_garbage_after_provenance() {
        let mut bytes = test_table_bytes_v2("ok");
        bytes.push(0);
        let crc = crc32fast::hash(&bytes[16..]);
        bytes[8..12].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            WeightTable::from_bytes(&bytes),
            Err(TableError::InvalidProvenance)
        ));
    }

    #[test]
    fn truncated_header_is_truncated_not_magic() {
        assert!(matches!(
            WeightTable::from_bytes(&[]),
            Err(TableError::Truncated(0))
        ));
        assert!(matches!(
            WeightTable::from_bytes(b"SPNG\x01\x00"),
            Err(TableError::Truncated(6))
        ));
    }

    #[test]
    fn weight_is_deterministic() {
        let bytes = test_table_bytes();
        let table = WeightTable::from_bytes(&bytes).unwrap();
        let w1 = table.weight(b'f', b'n');
        let w2 = table.weight(b'f', b'n');
        assert_eq!(w1, w2);
    }

    #[test]
    fn weight_matches_crc32_of_pair() {
        let bytes = test_table_bytes();
        let table = WeightTable::from_bytes(&bytes).unwrap();
        let expected = crc32fast::hash(b"fn");
        assert_eq!(table.weight(b'f', b'n'), expected);
    }

    #[test]
    fn rejects_wrong_size() {
        let err = WeightTable::from_bytes(&[0; 100]).unwrap_err();
        assert!(matches!(
            err,
            TableError::InvalidMagic | TableError::Truncated(_)
        ));
    }

    #[test]
    fn rejects_v1_with_extra_bytes() {
        let mut bytes = test_table_bytes();
        bytes.push(0);
        assert!(matches!(
            WeightTable::from_bytes(&bytes),
            Err(TableError::InvalidSize(_))
        ));
    }

    #[test]
    fn rejects_wrong_magic() {
        let mut bytes = test_table_bytes();
        bytes[0] = b'X';
        assert!(matches!(
            WeightTable::from_bytes(&bytes),
            Err(TableError::InvalidMagic)
        ));
    }

    #[test]
    fn rejects_bad_checksum() {
        let mut bytes = test_table_bytes();
        bytes[20] ^= 0xFF;
        assert!(matches!(
            WeightTable::from_bytes(&bytes),
            Err(TableError::Checksum { .. })
        ));
    }
}
