//! The pre-trained production sparse n-gram weight table, embedded at
//! compile time behind the `production` feature.

use sngram_types::WeightTable;

/// Load the embedded production table.
#[cfg(feature = "production")]
#[must_use]
#[allow(
    clippy::missing_panics_doc,
    clippy::panic,
    reason = "the build script validates the embedded table with the same parser"
)]
pub fn weights() -> WeightTable {
    match WeightTable::from_bytes(PRODUCTION_BYTES) {
        Ok(table) => table,
        Err(err) => unreachable!("build script validated embedded weight table: {err}"),
    }
}

#[cfg(feature = "production")]
const PRODUCTION_BYTES: &[u8] = include_bytes!("../data/final_weights.bin");

#[cfg(test)]
mod tests {
    #[cfg(feature = "production")]
    #[test]
    fn production_table_loads() {
        assert_ne!(super::weights().fingerprint(), 0);
    }
}
