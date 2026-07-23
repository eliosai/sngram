//! Weight table type.

use crate::error::TableError;
use crate::spng::{
    self, TableParts, WeightTableSettings, fingerprint_bytes, verify_checksum, write_weights,
};

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
            weights: spng::build_weights(&mut weight),
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
        let parts = TableParts::parse(bytes)?;
        verify_checksum(parts.expected_crc, parts.body)?;
        Ok(Self::from_parts(parts, fingerprint_bytes(bytes)))
    }

    #[doc(hidden)]
    pub fn from_prevalidated_bytes(bytes: &[u8], fingerprint: u64) -> Result<Self, TableError> {
        Ok(Self::from_parts(TableParts::parse(bytes)?, fingerprint))
    }

    fn from_parts(parts: TableParts<'_>, fingerprint: u64) -> Self {
        Self {
            weights: spng::parse_weights(parts.data),
            version: parts.version,
            provenance: parts.provenance,
            fingerprint,
        }
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
            let len = spng::provenance_len(provenance);
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
