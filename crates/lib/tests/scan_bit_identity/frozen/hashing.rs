//! Verbatim copy of the baseline rolling polynomial gram hashing

struct HashSettings;

impl HashSettings {
    const MAX_GRAM_HASH_LEN: usize = 100;
    const BASE: u64 = 0x9E37_79B9_7F4A_7C15;
    const FOLD_SALT: u64 = 0xF01D_5A17_C0DE_D00D;
    const POW: [u64; Self::MAX_GRAM_HASH_LEN + 1] = pow_table();
}

/// Copy of the baseline key space folded into the raw polynomial value
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct HashKey(u64);

impl HashKey {
    pub const UNKEYED: Self = Self(0);

    pub const fn folded(self) -> Self {
        Self(self.0 ^ HashSettings::FOLD_SALT)
    }

    pub const fn advance_prefix_hash(self, prefix_hash: u64, byte: u8) -> u64 {
        let Self(_) = self;
        prefix_hash
            .wrapping_mul(HashSettings::BASE)
            .wrapping_add(byte as u64)
    }

    pub const fn hash_from_prefixes(self, h_end: u64, h_before_start: u64, len: usize) -> u64 {
        mix(h_end.wrapping_sub(
            h_before_start
                .wrapping_sub(1)
                .wrapping_mul(HashSettings::POW[len]),
        ) ^ self.0)
    }
}

const fn pow_table() -> [u64; HashSettings::MAX_GRAM_HASH_LEN + 1] {
    let mut table = [1u64; HashSettings::MAX_GRAM_HASH_LEN + 1];
    let mut k = 1;
    while k <= HashSettings::MAX_GRAM_HASH_LEN {
        table[k] = table[k - 1].wrapping_mul(HashSettings::BASE);
        k += 1;
    }
    table
}

const fn mix(mut z: u64) -> u64 {
    z ^= z >> 30;
    z = z.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z ^= z >> 27;
    z = z.wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    z
}
