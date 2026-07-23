//! Pre-trained weight tables embedded per training-data tier
//!
//! Enable exactly one tier feature; enabling two fails to compile because
//! `weights_bytes` would be defined twice.

use sngram_types::WeightTable;

/// Load the embedded table selected by the enabled tier feature
#[must_use]
#[allow(
    clippy::missing_panics_doc,
    clippy::panic,
    reason = "the build script validates the embedded table with the same parser"
)]
pub fn weights() -> WeightTable {
    match WeightTable::from_prevalidated_bytes(weights_bytes(), weights_fingerprint()) {
        Ok(table) => table,
        Err(err) => unreachable!("build script validated embedded weight table: {err}"),
    }
}

macro_rules! embed_table {
    ($feature:literal, $bytes:ident, $path:literal) => {
        #[cfg(feature = $feature)]
        const $bytes: &[u8] = include_bytes!($path);

        #[cfg(feature = $feature)]
        const fn weights_bytes() -> &'static [u8] {
            $bytes
        }
    };
}

#[cfg(feature = "12tb")]
include!(concat!(env!("OUT_DIR"), "/12tb_weights.rs"));

#[cfg(feature = "12tb")]
const fn weights_fingerprint() -> u64 {
    WEIGHTS_12TB_FINGERPRINT
}

embed_table!("12tb", BYTES_12TB, "../data/12tb_weights.bin");

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_table_loads() {
        assert_ne!(super::weights().fingerprint(), 0);
    }
}
