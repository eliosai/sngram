//! Packed gram records and the slot table that folds their repeats.

/// Bits a packed gram reserves for its posting mask
const MASK_BITS: u32 = 10;

/// Odd 64-bit fraction of the golden ratio, spreading keys over slots
const SLOT_MIX: u64 = 0x9E37_79B9_7F4A_7C15;

/// Gram counts below this sort instead, where a slot table costs more than it
/// saves
const MIN_TABLE_GRAMS: usize = 1024;

/// One selected gram occurrence: the gram key shifted above its posting mask,
/// so one `u64` carries both and one `u64` sort groups repeats of a key
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct PackedGram(u64);

impl PackedGram {
    /// Every mask carries a line-bucket bit, so a packed gram is never zero
    pub const fn new(key: u64, mask: u16) -> Self {
        Self(key << MASK_BITS | mask as u64)
    }

    /// The indexed 32-bit hash of this gram
    pub const fn hash(self) -> u32 {
        (self.0 >> MASK_BITS) as u32
    }

    pub const fn mask(self) -> u16 {
        (self.0 & ((1 << MASK_BITS) - 1)) as u16
    }

    /// The stored width of a gram key, which packing shortens
    pub const fn truncate(key: u64) -> u64 {
        key & (u64::MAX >> MASK_BITS)
    }

    pub const fn key(self) -> u64 {
        self.0 >> MASK_BITS
    }
}

/// Reduce repeats of one key to a single record carrying the OR of their masks
pub fn collapse(grams: &mut Vec<PackedGram>) {
    if grams.len() < MIN_TABLE_GRAMS || !collapse_by_table(grams) {
        collapse_by_sort(grams);
    }
}

fn collapse_by_sort(grams: &mut Vec<PackedGram>) {
    grams.sort_unstable();
    grams.dedup_by(|next, kept| {
        if next.key() == kept.key() {
            kept.0 |= next.0;
            true
        } else {
            false
        }
    });
}

/// Fold grams through an open-addressed slot table, refusing documents whose
/// distinct grams would crowd it
fn collapse_by_table(grams: &mut Vec<PackedGram>) -> bool {
    let slot_count = (grams.len() / 2).next_power_of_two();
    let limit = slot_count - slot_count / 4;
    let mut slots = vec![0u64; slot_count];
    let mut used = 0usize;
    for gram in grams.iter() {
        used += usize::from(insert(&mut slots, slot_count - 1, gram.0));
        if used > limit {
            return false;
        }
    }
    let mut kept = 0usize;
    for &slot in &slots {
        if slot != 0 {
            grams[kept] = PackedGram(slot);
            kept += 1;
        }
    }
    grams.truncate(kept);
    true
}

/// Fold one gram into the slots, reporting whether it claimed a fresh one.
/// An empty slot reads zero, which no packed gram can be
fn insert(slots: &mut [u64], index_mask: usize, gram: u64) -> bool {
    let key = gram >> MASK_BITS;
    let mut at = (key.wrapping_mul(SLOT_MIX) >> 32) as usize & index_mask;
    loop {
        let slot = slots[at];
        if slot == 0 {
            slots[at] = gram;
            return true;
        }
        if slot >> MASK_BITS == key {
            slots[at] = slot | gram;
            return false;
        }
        at = (at + 1) & index_mask;
    }
}

#[cfg(test)]
mod tests {
    use super::{MIN_TABLE_GRAMS, PackedGram, collapse, collapse_by_sort};
    use std::collections::BTreeMap;

    fn packed(input: &[(u64, u16)]) -> Vec<PackedGram> {
        input
            .iter()
            .map(|&(key, mask)| PackedGram::new(key, mask))
            .collect()
    }

    fn folded(grams: Vec<PackedGram>) -> BTreeMap<u64, u16> {
        grams
            .into_iter()
            .map(|gram| (gram.key(), gram.mask()))
            .collect()
    }

    fn expected(input: &[(u64, u16)]) -> BTreeMap<u64, u16> {
        let mut want: BTreeMap<u64, u16> = BTreeMap::new();
        for &(key, mask) in input {
            *want.entry(PackedGram::truncate(key)).or_default() |= mask;
        }
        want
    }

    /// Pad a case out to the table path without disturbing its own keys
    fn padded(input: &[(u64, u16)]) -> Vec<(u64, u16)> {
        let mut out = input.to_vec();
        out.extend((0..MIN_TABLE_GRAMS as u64).map(|i| (1 << 50 | i, 1u16)));
        out
    }

    #[test]
    fn packing_round_trips_the_hash_and_mask() {
        let gram = PackedGram::new(0xDEAD_BEEF_CAFE_1234, 0x3FF);
        assert_eq!(gram.hash(), 0xCAFE_1234);
        assert_eq!(gram.mask(), 0x3FF);
        assert_eq!(gram.key(), PackedGram::truncate(0xDEAD_BEEF_CAFE_1234));
    }

    #[test]
    fn truncation_keeps_the_indexed_low_bits() {
        for key in [0u64, 1, u64::MAX, 0x8000_0000_0000_0001] {
            assert_eq!(PackedGram::new(key, 1).hash(), key as u32);
        }
    }

    #[test]
    fn both_paths_fold_repeats_of_one_key() {
        let input = [
            (7u64, 0b0_0001u16),
            (7, 0b1_0000),
            (9, 0b0_0010),
            (7, 0b100),
        ];
        let mut small = packed(&input);
        collapse(&mut small);
        assert_eq!(folded(small), expected(&input));

        let big = padded(&input);
        let mut table = packed(&big);
        collapse(&mut table);
        assert_eq!(folded(table), expected(&big));
    }

    #[test]
    fn keys_sharing_low_bits_stay_separate_records() {
        let input = [(1u64 << 40 | 5, 1u16), (5, 2), (1 << 41 | 5, 4)];
        let mut grams = packed(&padded(&input));
        collapse(&mut grams);
        let got = folded(grams);
        assert_eq!(got.get(&5), Some(&2));
        assert_eq!(got.len(), MIN_TABLE_GRAMS + 3);
    }

    #[test]
    fn keys_differing_only_above_the_stored_width_collapse() {
        let input = [(1u64 << 63 | 5, 1u16), (5, 2)];
        let mut grams = packed(&input);
        collapse(&mut grams);
        assert_eq!(folded(grams).get(&5), Some(&3));
    }

    #[test]
    fn a_crowded_table_falls_back_to_the_sort() {
        let input: Vec<(u64, u16)> = (0..4000u64)
            .map(|i| (i.wrapping_mul(SPREAD), (i % 1023 + 1) as u16))
            .collect();
        let mut grams = packed(&input);
        collapse(&mut grams);
        assert_eq!(folded(grams), expected(&input));
    }

    #[test]
    fn the_table_matches_the_sort_on_the_same_grams() {
        let input: Vec<(u64, u16)> = (0..40_000u64)
            .map(|i| (i.wrapping_mul(SPREAD) % 5_000, (i % 1023 + 1) as u16))
            .collect();
        let mut table = packed(&input);
        collapse(&mut table);
        let mut sorted = packed(&input);
        collapse_by_sort(&mut sorted);
        assert_eq!(folded(table), folded(sorted));
    }

    const SPREAD: u64 = 0x9E37_79B9_7F4A_7C15;
}
