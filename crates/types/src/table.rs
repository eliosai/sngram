//! Weight table type.

use crate::error::TableError;

struct WeightTableSettings;

impl WeightTableSettings {
    const HEADER_SIZE: usize = 16;
    const WEIGHTS_COUNT: usize = 65_536;
    const MAGIC: &[u8; 4] = b"SPNG";
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    /// Total v1 table binary size; v2 adds a provenance block after the weights.
    const TABLE_BINARY_SIZE: usize = Self::HEADER_SIZE + Self::WEIGHTS_COUNT * 4;

    /// Longest accepted provenance block, in bytes.
    const PROVENANCE_MAX: usize = 1024;
}

/// 256x256 character-pair weight table.
#[derive(Debug, Clone)]
pub struct WeightTable {
    weights: Box<[u32; WeightTableSettings::WEIGHTS_COUNT]>,
    version: u32,
    provenance: Option<String>,
    fingerprint: u64,
}

impl WeightTable {
    /// Build a table from a function over every byte pair.
    #[must_use]
    pub fn from_weight_fn(mut weight: impl FnMut(u8, u8) -> u32) -> Self {
        let mut table = Self {
            weights: build_weights(&mut weight),
            version: 1,
            provenance: None,
            fingerprint: 0,
        };
        table.fingerprint = fingerprint_bytes(&table.to_bytes());
        table
    }

    /// # Errors
    ///
    /// Returns `TableError` on malformed data, an unknown version, or a
    /// checksum mismatch.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, TableError> {
        if bytes.len() < WeightTableSettings::HEADER_SIZE {
            return Err(TableError::Truncated(bytes.len()));
        }
        if bytes.get(..4) != Some(WeightTableSettings::MAGIC.as_slice()) {
            return Err(TableError::InvalidMagic);
        }
        let version = read_u32_le(bytes, 4)?;
        let expected_crc = read_u32_le(bytes, 8)?;
        let body = bytes
            .get(WeightTableSettings::HEADER_SIZE..)
            .ok_or(TableError::Truncated(bytes.len()))?;
        let provenance = version_provenance(version, bytes.len(), body)?;
        verify_checksum(expected_crc, body)?;
        let data = body
            .get(..WeightTableSettings::WEIGHTS_COUNT * 4)
            .ok_or(TableError::Truncated(bytes.len()))?;
        Ok(Self {
            weights: parse_weights(data),
            version,
            provenance,
            fingerprint: fingerprint_bytes(bytes),
        })
    }

    /// Return a copy of this table in the `SPNG` binary format.
    ///
    /// The returned bytes are accepted by [`WeightTable::from_bytes`].
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = vec![0u8; WeightTableSettings::TABLE_BINARY_SIZE];
        let version = if self.provenance.is_some() {
            2u32
        } else {
            1u32
        };
        buf[..4].copy_from_slice(WeightTableSettings::MAGIC);
        buf[4..8].copy_from_slice(&version.to_le_bytes());
        write_weights(&self.weights, &mut buf);
        if let Some(provenance) = &self.provenance {
            let len = provenance_len(provenance);
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(provenance.as_bytes());
        }
        let crc = crc32fast::hash(&buf[WeightTableSettings::HEADER_SIZE..]);
        buf[8..12].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    /// Deterministic table identity for manifests and cache keys.
    ///
    /// This is not a cryptographic authenticity check; table payload integrity
    /// is validated by [`WeightTable::from_bytes`].
    #[must_use]
    pub const fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    /// Return this table with an embedded provenance record.
    ///
    /// # Errors
    ///
    /// Returns [`TableError::InvalidProvenance`] when the provenance record is
    /// too large for this table format.
    pub fn with_provenance(mut self, provenance: impl Into<String>) -> Result<Self, TableError> {
        let provenance = provenance.into();
        if provenance.len() > WeightTableSettings::PROVENANCE_MAX {
            return Err(TableError::InvalidProvenance);
        }
        self.version = 2;
        self.provenance = Some(provenance);
        self.fingerprint = fingerprint_bytes(&self.to_bytes());
        Ok(self)
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
    #[must_use]
    pub fn weight(&self, c1: u8, c2: u8) -> u32 {
        self.weights[usize::from(c1) << 8 | usize::from(c2)]
    }

    /// Full 256x256 weight matrix as a fixed-size array reference.
    #[must_use]
    pub fn matrix(&self) -> &[u32; WeightTableSettings::WEIGHTS_COUNT] {
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

fn fingerprint_bytes(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(WeightTableSettings::FNV_OFFSET, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(WeightTableSettings::FNV_PRIME)
        })
}

/// Version gate: v1 is the exact legacy size, v2 carries a provenance tail.
fn version_provenance(
    version: u32,
    total_len: usize,
    body: &[u8],
) -> Result<Option<String>, TableError> {
    match version {
        1 => {
            if total_len != WeightTableSettings::TABLE_BINARY_SIZE {
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
        .get(WeightTableSettings::WEIGHTS_COUNT * 4..)
        .ok_or(TableError::Truncated(body.len()))?;
    let len_bytes: [u8; 2] = tail
        .get(..2)
        .and_then(|s| s.try_into().ok())
        .ok_or(TableError::Truncated(tail.len()))?;
    let len = usize::from(u16::from_le_bytes(len_bytes));
    if len > WeightTableSettings::PROVENANCE_MAX {
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

#[allow(
    clippy::expect_used,
    reason = "provenance is validated to be smaller than u16::MAX"
)]
fn provenance_len(provenance: &str) -> u16 {
    u16::try_from(provenance.len()).expect("valid provenance length")
}

#[allow(
    clippy::expect_used,
    reason = "the vec has exactly WEIGHTS_COUNT elements; conversion cannot fail"
)]
fn zero_weights() -> Box<[u32; WeightTableSettings::WEIGHTS_COUNT]> {
    vec![0u32; WeightTableSettings::WEIGHTS_COUNT]
        .into_boxed_slice()
        .try_into()
        .expect("WEIGHTS_COUNT elements")
}

fn build_weights(
    weight: &mut impl FnMut(u8, u8) -> u32,
) -> Box<[u32; WeightTableSettings::WEIGHTS_COUNT]> {
    let mut weights = zero_weights();
    for (i, w) in weights.iter_mut().enumerate() {
        let [_, c1, c2] = low_pair_bytes(i);
        *w = weight(c1, c2);
    }
    weights
}

fn write_weights(weights: &[u32; WeightTableSettings::WEIGHTS_COUNT], buf: &mut [u8]) {
    let data = &mut buf[WeightTableSettings::HEADER_SIZE..];
    for (i, w) in weights.iter().enumerate() {
        let off = i * 4;
        data[off..off + 4].copy_from_slice(&w.to_le_bytes());
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "each value is masked to one byte before casting"
)]
const fn low_pair_bytes(index: usize) -> [u8; 3] {
    [
        ((index >> 16) & 0xFF) as u8,
        ((index >> 8) & 0xFF) as u8,
        (index & 0xFF) as u8,
    ]
}

fn parse_weights(data: &[u8]) -> Box<[u32; WeightTableSettings::WEIGHTS_COUNT]> {
    let mut weights = zero_weights();
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
        let mut buf = vec![0u8; WeightTableSettings::TABLE_BINARY_SIZE];
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
    fn builds_table_from_weight_function() {
        let table = WeightTable::from_weight_fn(|a, b| crc32fast::hash(&[a, b]));
        assert_eq!(table.version(), 1);
        assert_eq!(table.provenance(), None);
        assert_eq!(table.weight(b'f', b'n'), crc32fast::hash(b"fn"));
    }

    #[test]
    fn weight_indexes_first_and_last_pairs() {
        let table = WeightTable::from_weight_fn(|a, b| u32::from(a) << 8 | u32::from(b));

        assert_eq!(table.weight(0, 0), 0);
        assert_eq!(table.weight(0, u8::MAX), u32::from(u8::MAX));
        assert_eq!(table.weight(u8::MAX, 0), u32::from(u8::MAX) << 8);
        assert_eq!(table.weight(u8::MAX, u8::MAX), u32::from(u16::MAX));
    }

    #[test]
    fn to_bytes_round_trips_v1() {
        let table = WeightTable::from_weight_fn(|a, b| crc32fast::hash(&[a, b]));
        let bytes = table.to_bytes();
        let loaded = WeightTable::from_bytes(&bytes).unwrap();
        assert_eq!(loaded.version(), 1);
        assert_eq!(loaded.provenance(), None);
        assert_eq!(loaded.matrix(), table.matrix());
    }

    #[test]
    fn fingerprint_round_trips_with_table_bytes() {
        let table = WeightTable::from_bytes(&test_table_bytes_v2("corpus=test")).unwrap();
        let loaded = WeightTable::from_bytes(&table.to_bytes()).unwrap();
        assert_eq!(loaded.fingerprint(), table.fingerprint());
    }

    #[test]
    fn to_bytes_round_trips_v2() {
        let table = WeightTable::from_weight_fn(|a, b| crc32fast::hash(&[a, b]))
            .with_provenance("corpus=test")
            .unwrap();
        let bytes = table.to_bytes();
        let loaded = WeightTable::from_bytes(&bytes).unwrap();
        assert_eq!(loaded.version(), 2);
        assert_eq!(loaded.provenance(), Some("corpus=test"));
        assert_eq!(loaded.matrix(), table.matrix());
    }

    #[test]
    fn with_provenance_rejects_oversized_record() {
        let table = WeightTable::from_weight_fn(|_, _| 1);
        let big = "x".repeat(WeightTableSettings::PROVENANCE_MAX + 1);
        assert!(matches!(
            table.with_provenance(big),
            Err(TableError::InvalidProvenance)
        ));
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
        let big = "x".repeat(WeightTableSettings::PROVENANCE_MAX + 1);
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
