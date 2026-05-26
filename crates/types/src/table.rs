//! Weight table type.

use crate::error::TableError;

const HEADER_SIZE: usize = 16;
const WEIGHTS_COUNT: usize = 65_536;
const MAGIC: &[u8; 4] = b"SPNG";

/// Total table binary size.
pub const TABLE_BINARY_SIZE: usize = HEADER_SIZE + WEIGHTS_COUNT * 4;

/// 256x256 character-pair weight table.
#[derive(Debug, Clone)]
pub struct WeightTable {
    weights: Vec<u32>,
    version: u32,
}

impl WeightTable {
    /// # Errors
    ///
    /// Returns `TableError` on malformed data or checksum mismatch.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, TableError> {
        if bytes.len() != TABLE_BINARY_SIZE {
            return Err(TableError::InvalidSize(bytes.len()));
        }
        if bytes.get(..4) != Some(MAGIC.as_slice()) {
            return Err(TableError::InvalidMagic);
        }

        let version = read_u32_le(bytes, 4)?;
        let expected_crc = read_u32_le(bytes, 8)?;
        let data = bytes.get(HEADER_SIZE..).ok_or(TableError::InvalidMagic)?;
        let actual_crc = crc32fast::hash(data);

        if expected_crc != actual_crc {
            return Err(TableError::Checksum {
                expected: expected_crc,
                actual: actual_crc,
            });
        }

        Ok(Self { weights: parse_weights(data), version })
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    #[allow(clippy::indexing_slicing, reason = "u8<<8|u8 <= 65535 < 65536")]
    #[must_use]
    pub fn weight(&self, c1: u8, c2: u8) -> u32 {
        self.weights[usize::from(c1) << 8 | usize::from(c2)]
    }
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32, TableError> {
    bytes
        .get(offset..offset + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(TableError::InvalidMagic)
}

#[allow(clippy::indexing_slicing, reason = "data.len() == WEIGHTS_COUNT * 4")]
fn parse_weights(data: &[u8]) -> Vec<u32> {
    (0..WEIGHTS_COUNT)
        .map(|i| {
            let off = i * 4;
            u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_table_bytes() -> Vec<u8> {
        let mut buf = vec![0u8; TABLE_BINARY_SIZE];
        buf[..4].copy_from_slice(b"SPNG");
        buf[4..8].copy_from_slice(&1u32.to_le_bytes());

        for c1 in 0u16..256 {
            for c2 in 0u16..256 {
                let w = crc32fast::hash(&[c1 as u8, c2 as u8]);
                let idx = (c1 as usize) << 8 | c2 as usize;
                let off = 16 + idx * 4;
                buf[off..off + 4].copy_from_slice(&w.to_le_bytes());
            }
        }

        let crc = crc32fast::hash(&buf[16..]);
        buf[8..12].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    #[test]
    fn loads_valid_table() {
        let bytes = test_table_bytes();
        let table = WeightTable::from_bytes(&bytes).unwrap();
        assert_eq!(table.version(), 1);
        assert_ne!(table.weight(b'a', b'b'), 0);
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
        assert!(matches!(err, TableError::InvalidSize(100)));
    }

    #[test]
    fn rejects_wrong_magic() {
        let mut bytes = test_table_bytes();
        bytes[0] = b'X';
        assert!(matches!(WeightTable::from_bytes(&bytes), Err(TableError::InvalidMagic)));
    }

    #[test]
    fn rejects_bad_checksum() {
        let mut bytes = test_table_bytes();
        bytes[20] ^= 0xFF;
        assert!(matches!(WeightTable::from_bytes(&bytes), Err(TableError::Checksum { .. })));
    }
}
