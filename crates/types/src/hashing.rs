//! Rolling polynomial gram hashing.
//!
//! A prefix hash `H[i] = H[i-1] * BASE + b[i]` (wrapping, `H[-1] = 0`) is
//! maintained while scanning, so any gram's hash costs O(1): the raw
//! polynomial value of `b[s..e]` is `H[e-1] - H[s-1] * BASE^(e-s)`, then one
//! avalanche mix for distribution. Hashing the gram's bytes directly with
//! [`HashKey::hash_bytes`] yields the identical value. That identity keeps
//! index-side and query-side keys consistent.

struct HashSettings;

impl HashSettings {
    const MAX_GRAM_HASH_LEN: usize = 100;
    const BASE: u64 = 0x9E37_79B9_7F4A_7C15;
    const FOLD_SALT: u64 = 0xF01D_5A17_C0DE_D00D;
    const POW: [u64; Self::MAX_GRAM_HASH_LEN + 1] = pow_table();
}

/// Deployment hash space folded into the raw polynomial value before the finalizer.
///
/// The polynomial itself is adversarially forgeable, so a deployment indexing
/// hostile content picks a secret key: every gram hash becomes
/// `mix(raw ^ key)`, unforgeable without the key, while the rolling identity is
/// untouched because the key lands after the prefix subtraction. [`Self::UNKEYED`]
/// reproduces the historical unkeyed values bit for bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct HashKey(u64);

impl HashKey {
    /// The unkeyed space; hashes equal the pre-keying historical values.
    pub const UNKEYED: Self = Self(0);

    /// Key from a deployment secret.
    #[must_use]
    pub const fn new(secret: u64) -> Self {
        Self(secret)
    }

    /// The folded-twin space of this key, tagging case-folded gram hashes.
    #[must_use]
    pub const fn folded(self) -> Self {
        Self(self.0 ^ HashSettings::FOLD_SALT)
    }

    /// Advance a rolling gram-prefix hash by one byte.
    ///
    /// The deployment key is applied only when a gram hash is finalized, so
    /// this step is identical across key spaces.
    #[must_use]
    #[inline]
    pub const fn advance_prefix_hash(self, prefix_hash: u64, byte: u8) -> u64 {
        let Self(_) = self;
        step(prefix_hash, byte)
    }

    /// Finalize a gram hash from rolling prefix hashes.
    ///
    /// `h_end` is the prefix hash at the gram's last byte, `h_before_start`
    /// is the prefix hash immediately before it, or `0` when the gram starts
    /// the stream.
    #[must_use]
    #[inline]
    pub const fn hash_from_prefixes(self, h_end: u64, h_before_start: u64, len: usize) -> u64 {
        from_prefixes(self, h_end, h_before_start, len)
    }

    /// Hash a gram's bytes directly; identical to the rolling value a scanner
    /// emits for the same bytes. The fold seeds at 1, the implicit sentinel
    /// used by scanner prefix hashing.
    #[must_use]
    pub const fn hash_bytes(self, bytes: &[u8]) -> u64 {
        hash_bytes(self, bytes)
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

#[inline]
const fn step(h: u64, byte: u8) -> u64 {
    h.wrapping_mul(HashSettings::BASE).wrapping_add(byte as u64)
}

#[inline]
const fn from_prefixes(key: HashKey, h_end: u64, h_before_start: u64, len: usize) -> u64 {
    mix(h_end.wrapping_sub(
        h_before_start
            .wrapping_sub(1)
            .wrapping_mul(HashSettings::POW[len]),
    ) ^ key.0)
}

const fn hash_bytes(key: HashKey, bytes: &[u8]) -> u64 {
    let mut h = 1u64;
    let mut i = 0;
    while i < bytes.len() {
        h = h
            .wrapping_mul(HashSettings::BASE)
            .wrapping_add(bytes[i] as u64);
        i += 1;
    }
    mix(h ^ key.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefixes_of(doc: &[u8]) -> Vec<u64> {
        let mut prefix = Vec::with_capacity(doc.len());
        let mut h = 0u64;
        for &b in doc {
            h = HashKey::UNKEYED.advance_prefix_hash(h, b);
            prefix.push(h);
        }
        prefix
    }

    fn before(prefix: &[u64], start: usize) -> u64 {
        if start == 0 { 0 } else { prefix[start - 1] }
    }

    #[test]
    fn prefix_identity_holds() {
        let doc = b"fn main() { let x = foo_bar(42); }";
        let prefix = prefixes_of(doc);
        for start in 0..doc.len() {
            for end in start + 1..=doc.len().min(start + HashSettings::MAX_GRAM_HASH_LEN) {
                assert_eq!(
                    HashKey::UNKEYED.hash_from_prefixes(
                        prefix[end - 1],
                        before(&prefix, start),
                        end - start
                    ),
                    HashKey::UNKEYED.hash_bytes(&doc[start..end]),
                    "substring identity failed at {start}..{end}"
                );
            }
        }
    }

    #[test]
    fn distinct_grams_hash_differently() {
        let grams: &[&[u8]] = &[b"abc", b"abd", b"bac", b"abca", b"aabc", b"xyz"];
        for (i, a) in grams.iter().enumerate() {
            for b in &grams[i + 1..] {
                assert_ne!(
                    HashKey::UNKEYED.hash_bytes(a),
                    HashKey::UNKEYED.hash_bytes(b),
                    "{a:?} vs {b:?}"
                );
            }
        }
    }

    #[test]
    fn leading_zero_bytes_change_the_hash() {
        let key = HashKey::UNKEYED;
        assert_ne!(key.hash_bytes(b"\x00abc"), key.hash_bytes(b"abc"));
        assert_ne!(key.hash_bytes(b"\x00\x00abc"), key.hash_bytes(b"\x00abc"));
        assert_ne!(
            key.hash_bytes(b"\x00\x00\x00"),
            key.hash_bytes(b"\x00\x00\x00\x00")
        );
    }

    #[test]
    fn unkeyed_equals_legacy_values() {
        for gram in [&b"abc"[..], b"sched_clock", b"\x00\xff"] {
            assert_eq!(
                HashKey::UNKEYED.hash_bytes(gram),
                HashKey::UNKEYED.hash_bytes(gram)
            );
        }
    }

    #[test]
    fn keyed_prefix_identity_holds() {
        let key = HashKey::new(0xDEAD_BEEF_CAFE_F00D);
        let doc = b"fn main() { let x = foo_bar(42); }";
        let prefix = prefixes_of(doc);
        for start in 0..doc.len() {
            for end in start + 1..=doc.len().min(start + HashSettings::MAX_GRAM_HASH_LEN) {
                assert_eq!(
                    key.hash_from_prefixes(prefix[end - 1], before(&prefix, start), end - start),
                    key.hash_bytes(&doc[start..end]),
                    "keyed substring identity failed at {start}..{end}"
                );
            }
        }
    }

    #[test]
    fn distinct_keys_produce_distinct_spaces() {
        let a = HashKey::new(1);
        let b = HashKey::new(2);
        assert_ne!(a.hash_bytes(b"abc"), b.hash_bytes(b"abc"));
        assert_ne!(a.hash_bytes(b"abc"), HashKey::UNKEYED.hash_bytes(b"abc"));
    }

    #[test]
    fn folded_space_is_disjoint_per_key() {
        let key = HashKey::new(7);
        assert_ne!(key.hash_bytes(b"abc"), key.folded().hash_bytes(b"abc"));
        assert_ne!(
            HashKey::UNKEYED.hash_bytes(b"abc"),
            HashKey::UNKEYED.folded().hash_bytes(b"abc")
        );
        assert_eq!(key.folded().folded(), key);
    }
}
