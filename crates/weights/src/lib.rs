//! Pre-trained sparse n-gram weight tables, embedded at compile time.
//!
//! Enable one or more table features to embed the official 0.5 tables:
//! `500gb`, `1tb`, `2tb`, `3tb`, `4tb`, `5tb`, or `10tb`.
//!
//! Use [`available`] and [`get`] when a caller accepts a table name at runtime.
//! Use [`weights`] when the crate is compiled with exactly one table feature.

pub use sngram_types::{TABLE_BINARY_SIZE, TableError, WeightTable};

/// Stable metadata for an embedded weight table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltinTable {
    id: &'static str,
    training_bytes: u64,
    bytes: &'static [u8],
    fingerprint: u64,
}

impl BuiltinTable {
    /// Feature/table id, for example `5tb`.
    #[must_use]
    pub const fn id(self) -> &'static str {
        self.id
    }

    /// Approximate number of training bytes represented by this table.
    #[must_use]
    pub const fn training_bytes(self) -> u64 {
        self.training_bytes
    }

    /// Deterministic 64-bit table fingerprint for index manifests.
    ///
    /// This is an identity guard, not a cryptographic authenticity check.
    #[must_use]
    pub const fn fingerprint(self) -> u64 {
        self.fingerprint
    }

    /// Raw table bytes in the `SPNG` table format, v1 or v2.
    #[must_use]
    pub const fn bytes(self) -> &'static [u8] {
        self.bytes
    }

    /// Parse and validate the embedded table.
    ///
    /// # Errors
    ///
    /// Returns [`TableError`] if the embedded bytes are malformed or fail their
    /// payload checksum.
    pub fn load(self) -> Result<WeightTable, TableError> {
        WeightTable::from_bytes(self.bytes)
    }
}

/// Errors from selecting a single default embedded table.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WeightsError {
    /// No table feature was enabled.
    #[error("no sngram weight table feature is enabled")]
    NoTableEnabled,
    /// More than one table feature was enabled.
    #[error("{count} sngram weight table features are enabled")]
    MultipleTablesEnabled {
        /// Number of embedded tables.
        count: usize,
    },
    /// The embedded table failed validation.
    #[error("invalid embedded sngram weight table: {0}")]
    InvalidTable(#[from] TableError),
}

/// Return the embedded tables enabled for this build.
#[must_use]
pub const fn available() -> &'static [BuiltinTable] {
    AVAILABLE
}

/// Return an embedded table by id.
///
/// Matching is exact and ASCII-case-sensitive. Accepted ids are the same as
/// the Cargo feature names: `500gb`, `1tb`, `2tb`, `3tb`, `4tb`, `5tb`, `10tb`.
#[must_use]
pub fn get(id: &str) -> Option<BuiltinTable> {
    available().iter().copied().find(|table| table.id == id)
}

/// Return the only embedded table enabled for this build.
///
/// # Errors
///
/// Returns [`WeightsError::NoTableEnabled`] when no table feature is enabled,
/// or [`WeightsError::MultipleTablesEnabled`] when the build embeds more than
/// one table.
pub fn selected() -> Result<BuiltinTable, WeightsError> {
    match available() {
        [] => Err(WeightsError::NoTableEnabled),
        [table] => Ok(*table),
        tables => Err(WeightsError::MultipleTablesEnabled {
            count: tables.len(),
        }),
    }
}

/// Load the only embedded table enabled for this build.
///
/// # Errors
///
/// Returns [`WeightsError`] when table selection is ambiguous or validation
/// fails.
pub fn weights() -> Result<WeightTable, WeightsError> {
    Ok(selected()?.load()?)
}

/// Compute the same deterministic 64-bit fingerprint stored in [`BuiltinTable`].
///
/// This uses FNV-1a over the full table binary. It is intentionally simple and
/// stable so index manifests can compare table identity without reparsing the
/// matrix or depending on a cryptographic hash crate.
#[must_use]
#[allow(
    clippy::indexing_slicing,
    reason = "i is guarded by i < bytes.len() in the const loop"
)]
pub const fn fingerprint_bytes(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut i = 0usize;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    hash
}

#[cfg(feature = "500gb")]
const BYTES_500GB: &[u8] = include_bytes!("../data/500gb_weights.bin");
#[cfg(feature = "1tb")]
const BYTES_1TB: &[u8] = include_bytes!("../data/1tb_weights.bin");
#[cfg(feature = "2tb")]
const BYTES_2TB: &[u8] = include_bytes!("../data/2tb_weights.bin");
#[cfg(feature = "3tb")]
const BYTES_3TB: &[u8] = include_bytes!("../data/3tb_weights.bin");
#[cfg(feature = "4tb")]
const BYTES_4TB: &[u8] = include_bytes!("../data/4tb_weights.bin");
#[cfg(feature = "5tb")]
const BYTES_5TB: &[u8] = include_bytes!("../data/5tb_weights.bin");
#[cfg(feature = "10tb")]
const BYTES_10TB: &[u8] = include_bytes!("../data/10tb_weights.bin");

#[cfg(feature = "500gb")]
/// Official 500 GB sparse n-gram weight table.
pub const TABLE_500GB: BuiltinTable = BuiltinTable {
    id: "500gb",
    training_bytes: 500_000_000_000,
    bytes: BYTES_500GB,
    fingerprint: fingerprint_bytes(BYTES_500GB),
};

#[cfg(feature = "1tb")]
/// Official 1 TB sparse n-gram weight table.
pub const TABLE_1TB: BuiltinTable = BuiltinTable {
    id: "1tb",
    training_bytes: 1_000_000_000_000,
    bytes: BYTES_1TB,
    fingerprint: fingerprint_bytes(BYTES_1TB),
};

#[cfg(feature = "2tb")]
/// Official 2 TB sparse n-gram weight table.
pub const TABLE_2TB: BuiltinTable = BuiltinTable {
    id: "2tb",
    training_bytes: 2_000_000_000_000,
    bytes: BYTES_2TB,
    fingerprint: fingerprint_bytes(BYTES_2TB),
};

#[cfg(feature = "3tb")]
/// Official 3 TB sparse n-gram weight table.
pub const TABLE_3TB: BuiltinTable = BuiltinTable {
    id: "3tb",
    training_bytes: 3_000_000_000_000,
    bytes: BYTES_3TB,
    fingerprint: fingerprint_bytes(BYTES_3TB),
};

#[cfg(feature = "4tb")]
/// Official 4 TB sparse n-gram weight table.
pub const TABLE_4TB: BuiltinTable = BuiltinTable {
    id: "4tb",
    training_bytes: 4_000_000_000_000,
    bytes: BYTES_4TB,
    fingerprint: fingerprint_bytes(BYTES_4TB),
};

#[cfg(feature = "5tb")]
/// Official 5 TB sparse n-gram weight table.
pub const TABLE_5TB: BuiltinTable = BuiltinTable {
    id: "5tb",
    training_bytes: 5_000_000_000_000,
    bytes: BYTES_5TB,
    fingerprint: fingerprint_bytes(BYTES_5TB),
};

#[cfg(feature = "10tb")]
/// Official 10 TB sparse n-gram weight table.
pub const TABLE_10TB: BuiltinTable = BuiltinTable {
    id: "10tb",
    training_bytes: 10_000_000_000_000,
    bytes: BYTES_10TB,
    fingerprint: fingerprint_bytes(BYTES_10TB),
};

const AVAILABLE: &[BuiltinTable] = &[
    #[cfg(feature = "500gb")]
    TABLE_500GB,
    #[cfg(feature = "1tb")]
    TABLE_1TB,
    #[cfg(feature = "2tb")]
    TABLE_2TB,
    #[cfg(feature = "3tb")]
    TABLE_3TB,
    #[cfg(feature = "4tb")]
    TABLE_4TB,
    #[cfg(feature = "5tb")]
    TABLE_5TB,
    #[cfg(feature = "10tb")]
    TABLE_10TB,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_tables_are_valid() {
        for table in available() {
            assert!(table.bytes().len() >= TABLE_BINARY_SIZE);
            assert_eq!(table.fingerprint(), fingerprint_bytes(table.bytes()));
            table.load().unwrap();
        }
    }

    #[test]
    fn get_finds_enabled_tables() {
        for table in available() {
            assert_eq!(get(table.id()), Some(*table));
        }
        assert_eq!(get("not-a-table"), None);
    }

    #[test]
    fn selected_matches_feature_count() {
        match available() {
            [] => {
                assert!(matches!(selected(), Err(WeightsError::NoTableEnabled)));
                assert!(matches!(weights(), Err(WeightsError::NoTableEnabled)));
            },
            [table] => {
                assert_eq!(selected().unwrap(), *table);
                assert_eq!(
                    weights().unwrap().version(),
                    table.load().unwrap().version()
                );
            },
            tables => {
                assert!(matches!(
                    selected(),
                    Err(WeightsError::MultipleTablesEnabled { count }) if count == tables.len()
                ));
                assert!(matches!(
                    weights(),
                    Err(WeightsError::MultipleTablesEnabled { count }) if count == tables.len()
                ));
            },
        }
    }
}
