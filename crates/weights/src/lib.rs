//! Pre-trained sparse n-gram weight table, embedded at compile time.
//!
//! Each table is a 256x256 grid of byte-pair weights learned by streaming a
//! fixed volume of source code. That volume is the size. Pick a size with a
//! Cargo feature; [`weights`] returns the embedded table.
//!
//! ```toml
//! [dependencies]
//! sngram-weights = { version = "0.2", default-features = false, features = ["10tb"] }
//! ```
//!
//! Minted sizes: `1gb`, `10gb`, `50gb`, `100gb`, `1tb`, `5tb`, `10tb`. Enabling
//! a size that has not been minted yet fails the build with a clear message,
//! never a silent "unknown feature".

use std::sync::OnceLock;

pub use sngram_types::WeightTable;

macro_rules! unminted {
    ($feat:literal) => {
        #[cfg(feature = $feat)]
        compile_error!(concat!(
            "sngram-weights: size `",
            $feat,
            "` is not minted yet. Enable a minted size: 1gb, 10gb, 50gb, 100gb, 1tb, 5tb, 10tb."
        ));
    };
}

unminted!("25tb");
unminted!("30tb");
unminted!("40tb");
unminted!("45tb");

#[cfg(feature = "15tb")]
const BYTES: &[u8] = include_bytes!("../bins/15tb_weights.bin");

#[cfg(all(feature = "10tb", not(any(feature = "15tb"))))]
const BYTES: &[u8] = include_bytes!("../bins/10tb_weights.bin");

#[cfg(all(feature = "5tb", not(any(feature = "10tb", feature = "15tb"))))]
const BYTES: &[u8] = include_bytes!("../bins/5tb_weights.bin");

#[cfg(all(feature = "1tb", not(any(feature = "10tb", feature = "5tb"))))]
const BYTES: &[u8] = include_bytes!("../bins/1tb_weights.bin");
#[cfg(all(
    feature = "100gb",
    not(any(feature = "10tb", feature = "5tb", feature = "1tb"))
))]
const BYTES: &[u8] = include_bytes!("../bins/100gb_weights.bin");
#[cfg(all(
    feature = "50gb",
    not(any(feature = "10tb", feature = "5tb", feature = "1tb", feature = "100gb"))
))]
const BYTES: &[u8] = include_bytes!("../bins/50gb_weights.bin");
#[cfg(all(
    feature = "10gb",
    not(any(
        feature = "10tb",
        feature = "5tb",
        feature = "1tb",
        feature = "100gb",
        feature = "50gb"
    ))
))]
const BYTES: &[u8] = include_bytes!("../bins/10gb_weights.bin");
#[cfg(all(
    feature = "1gb",
    not(any(
        feature = "10tb",
        feature = "5tb",
        feature = "1tb",
        feature = "100gb",
        feature = "50gb",
        feature = "10gb"
    ))
))]
const BYTES: &[u8] = include_bytes!("../bins/1gb_weights.bin");

/// The embedded weight table for the enabled size feature.
#[must_use]
#[allow(
    clippy::expect_used,
    reason = "bytes are CRC-validated when minted; a failure here is a build bug"
)]
pub fn weights() -> &'static WeightTable {
    static CELL: OnceLock<WeightTable> = OnceLock::new();
    CELL.get_or_init(|| WeightTable::from_bytes(BYTES).expect("embedded weight table is malformed"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn loads_and_caches() {
        let a = super::weights();
        let b = super::weights();
        assert!(std::ptr::eq(a, b), "must return the same singleton");
        assert_ne!(
            a.weight(b'f', b'n'),
            a.weight(0, 0),
            "table looks degenerate"
        );
    }
}
