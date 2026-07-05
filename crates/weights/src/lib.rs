//! Pre-trained sparse n-gram weight tables, embedded at compile time.
//!
//! Enable exactly one table feature to embed an official 0.5 table:
//! `500gb`, `1tb`, `2tb`, `3tb`, `4tb`, `5tb`, `6tb`, `7tb`, `8tb`, `9tb`,
//! `10tb`, `11tb`, or `12tb`.

use sngram_types::WeightTable;

/// Load the embedded table selected by this crate's enabled feature.
#[cfg(any(
    feature = "500gb",
    feature = "1tb",
    feature = "2tb",
    feature = "3tb",
    feature = "4tb",
    feature = "5tb",
    feature = "6tb",
    feature = "7tb",
    feature = "8tb",
    feature = "9tb",
    feature = "10tb",
    feature = "11tb",
    feature = "12tb",
))]
#[must_use]
#[allow(
    clippy::missing_panics_doc,
    clippy::panic,
    reason = "the build script validates the selected embedded table with the same parser"
)]
pub fn weights() -> WeightTable {
    match WeightTable::from_bytes(weights_bytes()) {
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

embed_table!("500gb", BYTES_500GB, "../data/500gb_weights.bin");
embed_table!("1tb", BYTES_1TB, "../data/1tb_weights.bin");
embed_table!("2tb", BYTES_2TB, "../data/2tb_weights.bin");
embed_table!("3tb", BYTES_3TB, "../data/3tb_weights.bin");
embed_table!("4tb", BYTES_4TB, "../data/4tb_weights.bin");
embed_table!("5tb", BYTES_5TB, "../data/5tb_weights.bin");
embed_table!("6tb", BYTES_6TB, "../data/6tb_weights.bin");
embed_table!("7tb", BYTES_7TB, "../data/7tb_weights.bin");
embed_table!("8tb", BYTES_8TB, "../data/8tb_weights.bin");
embed_table!("9tb", BYTES_9TB, "../data/9tb_weights.bin");
embed_table!("10tb", BYTES_10TB, "../data/10tb_weights.bin");
embed_table!("11tb", BYTES_11TB, "../data/11tb_weights.bin");
embed_table!("12tb", BYTES_12TB, "../data/final_weights.bin");

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(
        feature = "500gb",
        feature = "1tb",
        feature = "2tb",
        feature = "3tb",
        feature = "4tb",
        feature = "5tb",
        feature = "6tb",
        feature = "7tb",
        feature = "8tb",
        feature = "9tb",
        feature = "10tb",
        feature = "11tb",
        feature = "12tb",
    ))]
    #[test]
    fn selected_table_loads() {
        assert_ne!(weights().fingerprint(), 0);
    }
}
