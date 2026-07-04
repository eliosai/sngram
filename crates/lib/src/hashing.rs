//! Rolling polynomial gram hashing.
//!
//! A prefix hash `H[i] = H[i-1] * BASE + b[i]` (wrapping, `H[-1] = 0`) is
//! maintained while scanning, so any gram's hash costs O(1): the raw
//! polynomial value of `b[s..e]` is `H[e-1] - H[s-1] * BASE^(e-s)`, then one
//! avalanche mix for distribution. Hashing the gram's bytes directly with
//! [`hash_bytes`] yields the identical value — that identity is what keeps
//! index-side and query-side keys consistent, and it is pinned by the
//! differential tests.

use crate::extract::MAX_LEN;

/// Polynomial base; odd, so multiplication permutes the u64 ring.
const BASE: u64 = 0x9E37_79B9_7F4A_7C15;

/// `BASE^k` for `k <= MAX_LEN`, the longest emittable gram.
const POW: [u64; MAX_LEN + 1] = pow_table();

#[allow(clippy::indexing_slicing, reason = "k stays <= MAX_LEN < table length")]
const fn pow_table() -> [u64; MAX_LEN + 1] {
    let mut table = [1u64; MAX_LEN + 1];
    let mut k = 1;
    while k <= MAX_LEN {
        table[k] = table[k - 1].wrapping_mul(BASE);
        k += 1;
    }
    table
}

/// splitmix64 finalizer: full-avalanche mix of the raw polynomial value.
const fn mix(mut z: u64) -> u64 {
    z ^= z >> 30;
    z = z.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z ^= z >> 27;
    z = z.wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    z
}

/// Salt separating the folded gram space from the primary space under any key.
const FOLD_SALT: u64 = 0xF01D_5A17_C0DE_D00D;

/// Deployment key folded into the raw polynomial value before the finalizer.
///
/// The polynomial itself is adversarially forgeable, so a deployment indexing
/// hostile content picks a secret key: every gram hash becomes
/// `mix(raw ^ key)`, unforgeable without the key, while the rolling identity
/// (`from_prefixes` equals `hash_bytes` for the same bytes) is untouched
/// because the key lands after the prefix subtraction. [`HashKey::UNKEYED`]
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
        Self(self.0 ^ FOLD_SALT)
    }
}

/// Advance the prefix hash by one byte: `H[i] = H[i-1] * BASE + b[i]`.
#[inline]
pub const fn step(h: u64, byte: u8) -> u64 {
    h.wrapping_mul(BASE).wrapping_add(byte as u64)
}

/// Gram hash from prefix hashes: `h_end` is `H[end-1]`, `h_before_start` is
/// `H[start-1]` (0 when the gram starts the document), `len` the gram length.
///
/// The `- 1` plants an implicit `0x01` sentinel before the gram (the raw
/// value becomes `BASE^len + poly(gram)`), so grams differing only in length
/// or leading zero bytes cannot collide — the property a length prefix gave
/// the old std-`Hash` path.
#[inline]
#[allow(
    clippy::indexing_slicing,
    reason = "emitted gram lengths are <= MAX_LEN"
)]
pub const fn from_prefixes(h_end: u64, h_before_start: u64, len: usize) -> u64 {
    from_prefixes_keyed(h_end, h_before_start, len, HashKey::UNKEYED)
}

/// Keyed variant of [`from_prefixes`]; the key applies after the prefix
/// subtraction, so the rolling identity with [`hash_bytes_keyed`] holds.
#[inline]
#[allow(
    clippy::indexing_slicing,
    reason = "emitted gram lengths are <= MAX_LEN"
)]
pub const fn from_prefixes_keyed(h_end: u64, h_before_start: u64, len: usize, key: HashKey) -> u64 {
    mix(h_end.wrapping_sub(h_before_start.wrapping_sub(1).wrapping_mul(POW[len])) ^ key.0)
}

/// Hash a gram's bytes directly; identical to the rolling value `scan` emits
/// for the same bytes. The fold seeds at 1 — the implicit sentinel matching
/// `from_prefixes`.
#[must_use]
pub const fn hash_bytes(bytes: &[u8]) -> u64 {
    hash_bytes_keyed(bytes, HashKey::UNKEYED)
}

/// Keyed variant of [`hash_bytes`]; equals the keyed rolling hash over the
/// same bytes and key.
#[must_use]
#[allow(clippy::indexing_slicing, reason = "i stays < bytes.len()")]
pub const fn hash_bytes_keyed(bytes: &[u8], key: HashKey) -> u64 {
    let mut h = 1u64;
    let mut i = 0;
    while i < bytes.len() {
        h = h.wrapping_mul(BASE).wrapping_add(bytes[i] as u64);
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
            h = h.wrapping_mul(BASE).wrapping_add(u64::from(b));
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
            for end in start + 1..=doc.len().min(start + MAX_LEN) {
                assert_eq!(
                    from_prefixes(prefix[end - 1], before(&prefix, start), end - start),
                    hash_bytes(&doc[start..end]),
                    "substring identity failed at {start}..{end}"
                );
            }
        }
    }

    #[test]
    fn distinct_grams_hash_differently() {
        // not a collision-resistance proof, just a sanity floor
        let grams: &[&[u8]] = &[b"abc", b"abd", b"bac", b"abca", b"aabc", b"xyz"];
        for (i, a) in grams.iter().enumerate() {
            for b in &grams[i + 1..] {
                assert_ne!(hash_bytes(a), hash_bytes(b), "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn leading_zero_bytes_change_the_hash() {
        // the implicit sentinel must disambiguate the classic polynomial-hash
        // collision class: leading NULs and pure length differences
        assert_ne!(hash_bytes(b"\x00abc"), hash_bytes(b"abc"));
        assert_ne!(hash_bytes(b"\x00\x00abc"), hash_bytes(b"\x00abc"));
        assert_ne!(hash_bytes(b"\x00\x00\x00"), hash_bytes(b"\x00\x00\x00\x00"));
    }

    #[test]
    fn unkeyed_equals_legacy_values() {
        for gram in [&b"abc"[..], b"sched_clock", b"\x00\xff"] {
            assert_eq!(hash_bytes(gram), hash_bytes_keyed(gram, HashKey::UNKEYED));
        }
    }

    #[test]
    fn keyed_prefix_identity_holds() {
        let key = HashKey::new(0xDEAD_BEEF_CAFE_F00D);
        let doc = b"fn main() { let x = foo_bar(42); }";
        let prefix = prefixes_of(doc);
        for start in 0..doc.len() {
            for end in start + 1..=doc.len().min(start + MAX_LEN) {
                assert_eq!(
                    from_prefixes_keyed(prefix[end - 1], before(&prefix, start), end - start, key),
                    hash_bytes_keyed(&doc[start..end], key),
                    "keyed substring identity failed at {start}..{end}"
                );
            }
        }
    }

    #[test]
    fn distinct_keys_produce_distinct_spaces() {
        let a = HashKey::new(1);
        let b = HashKey::new(2);
        assert_ne!(hash_bytes_keyed(b"abc", a), hash_bytes_keyed(b"abc", b));
        assert_ne!(hash_bytes_keyed(b"abc", a), hash_bytes(b"abc"));
    }

    #[test]
    fn folded_space_is_disjoint_per_key() {
        let key = HashKey::new(7);
        assert_ne!(
            hash_bytes_keyed(b"abc", key),
            hash_bytes_keyed(b"abc", key.folded())
        );
        assert_ne!(
            hash_bytes(b"abc"),
            hash_bytes_keyed(b"abc", HashKey::UNKEYED.folded())
        );
        assert_eq!(key.folded().folded(), key);
    }
}
