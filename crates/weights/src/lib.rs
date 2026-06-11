//! Pre-trained sparse n-gram weight tables, embedded at compile time.
//!
//! Each table is a 256x256 grid of byte-pair weights learned by streaming a
//! fixed volume of blended text (code + multilingual web). That volume is the
//! size. Pick a size with a Cargo feature; `weights()` returns the embedded
//! table.
//!
//! **No sizes are minted yet for 0.5.** The 0.4-era tables were retired with
//! the 0.5 training regime (blended corpus, new mint schedule); the `sngram`
//! Python trainer is producing the new set: `100gb`, `500gb`, `1tb`, then
//! every 5 TB up to `50tb`. Enabling any size before its table lands fails
//! the build with a clear message, never a silent "unknown feature". Until
//! then, train your own with `sngram train` and load it via
//! [`WeightTable::from_bytes`].

pub use sngram_types::WeightTable;

macro_rules! unminted {
    ($feat:literal) => {
        #[cfg(feature = $feat)]
        compile_error!(concat!(
            "sngram-weights: size `",
            $feat,
            "` is not minted yet for 0.5. The new tables are being trained; ",
            "until they land, mint your own with `sngram train` and load it ",
            "with WeightTable::from_bytes."
        ));
    };
}

unminted!("100gb");
unminted!("500gb");
unminted!("1tb");
unminted!("5tb");
unminted!("10tb");
unminted!("15tb");
unminted!("20tb");
unminted!("25tb");
unminted!("30tb");
unminted!("35tb");
unminted!("40tb");
unminted!("45tb");
unminted!("50tb");
