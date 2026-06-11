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
#[allow(clippy::indexing_slicing, reason = "emitted gram lengths are <= MAX_LEN")]
pub const fn from_prefixes(h_end: u64, h_before_start: u64, len: usize) -> u64 {
    mix(h_end.wrapping_sub(h_before_start.wrapping_sub(1).wrapping_mul(POW[len])))
}

/// Hash a gram's bytes directly; identical to the rolling value `scan` emits
/// for the same bytes. The fold seeds at 1 — the implicit sentinel matching
/// [`from_prefixes`].
#[must_use]
#[allow(clippy::indexing_slicing, reason = "i stays < bytes.len()")]
pub const fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h = 1u64;
    let mut i = 0;
    while i < bytes.len() {
        h = h.wrapping_mul(BASE).wrapping_add(bytes[i] as u64);
        i += 1;
    }
    mix(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_identity_holds() {
        let doc = b"fn main() { let x = foo_bar(42); }";
        // prefix hashes over the whole document
        let mut prefix = Vec::with_capacity(doc.len());
        let mut h = 0u64;
        for &b in doc {
            h = h.wrapping_mul(BASE).wrapping_add(u64::from(b));
            prefix.push(h);
        }
        for start in 0..doc.len() {
            for end in start + 1..=doc.len().min(start + MAX_LEN) {
                let h_before = if start == 0 { 0 } else { prefix[start - 1] };
                assert_eq!(
                    from_prefixes(prefix[end - 1], h_before, end - start),
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
}
