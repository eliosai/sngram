//! The pre-trained weight table embedded behind the weights feature

use sngram_types::WeightTable;

include!(concat!(env!("OUT_DIR"), "/weights.rs"));

const BYTES: &[u8] = include_bytes!("../data/weights.bin");

/// Load the embedded weight table
#[must_use]
#[allow(
    clippy::missing_panics_doc,
    clippy::panic,
    reason = "the build script validates the embedded table with the same parser"
)]
pub fn weights() -> WeightTable {
    match WeightTable::from_prevalidated_bytes(BYTES, WEIGHTS_FINGERPRINT) {
        Ok(table) => table,
        Err(err) => unreachable!("build script validated embedded weight table: {err}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_table_loads() {
        assert_ne!(super::weights().fingerprint(), 0);
    }
}
