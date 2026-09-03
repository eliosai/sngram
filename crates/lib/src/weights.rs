//! The trained production weight table embedded behind the weights feature

use std::sync::LazyLock;

use crate::WeightTable;

const BYTES: &[u8] = include_bytes!("../data/weights.bin");

static TABLE: LazyLock<WeightTable> = LazyLock::new(|| {
    WeightTable::from_bytes(BYTES)
        .unwrap_or_else(|err| unreachable!("the embedded weight table failed to parse: {err}"))
});

/// The embedded production weight table, parsed and checksummed on first use
#[must_use]
pub fn weights() -> &'static WeightTable {
    &TABLE
}

#[cfg(test)]
mod tests {
    use super::{BYTES, weights};
    use crate::WeightTable;

    #[test]
    fn the_embedded_bytes_parse_with_their_checksum() {
        let table = WeightTable::from_bytes(BYTES).expect("embedded table parses");
        assert_eq!(table.fingerprint(), weights().fingerprint());
        assert_ne!(table.fingerprint(), 0);
    }
}
