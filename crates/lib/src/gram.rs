//! Inline small-buffer gram type.
//!
//! Grams are 3–13 bytes in practice (bounded by `MAX_LEN = 100`), and the
//! query path was measured allocation-bound: every gram in a `Vec<u8>` was a
//! separate heap box. `Gram` stores up to [`INLINE_CAP`] bytes inline and
//! spills longer ones to the heap, eliminating the per-gram allocation for
//! the overwhelmingly common case. Representation is canonical (inline iff it
//! fits), so equality and ordering are plain byte comparisons.

use core::borrow::Borrow;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::ops::Deref;

use crate::hashing;

/// Longest gram stored inline; chosen so `size_of::<Gram>() == 24`, the same
/// footprint as the `Vec<u8>` it replaces.
const INLINE_CAP: usize = 22;

/// A gram: a short byte string with inline storage.
///
/// Dereferences to `[u8]`; compares, orders, and std-hashes by its bytes.
/// [`Gram::hash`] is the 64-bit index key, identical to the hash
/// [`crate::scan`] emits for the same bytes.
#[derive(Clone)]
pub struct Gram(Repr);

#[derive(Clone)]
enum Repr {
    Inline { len: u8, buf: [u8; INLINE_CAP] },
    Heap(Box<[u8]>),
}

impl Gram {
    /// The empty gram.
    #[must_use]
    pub const fn empty() -> Self {
        Self(Repr::Inline {
            len: 0,
            buf: [0; INLINE_CAP],
        })
    }

    /// Concatenation of two byte strings as one gram, without an intermediate
    /// allocation when the result fits inline.
    #[must_use]
    #[allow(clippy::indexing_slicing, reason = "a.len() + b.len() <= INLINE_CAP in the inline arm")]
    #[allow(clippy::cast_possible_truncation, reason = "inline arm length <= INLINE_CAP < 256")]
    pub fn concat(a: &[u8], b: &[u8]) -> Self {
        let n = a.len() + b.len();
        if n <= INLINE_CAP {
            let mut buf = [0u8; INLINE_CAP];
            buf[..a.len()].copy_from_slice(a);
            buf[a.len()..n].copy_from_slice(b);
            Self(Repr::Inline { len: n as u8, buf })
        } else {
            let mut v = Vec::with_capacity(n);
            v.extend_from_slice(a);
            v.extend_from_slice(b);
            Self(Repr::Heap(v.into_boxed_slice()))
        }
    }

    /// The gram's bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        match &self.0 {
            #[allow(clippy::indexing_slicing, reason = "len <= INLINE_CAP by construction")]
            Repr::Inline { len, buf } => &buf[..usize::from(*len)],
            Repr::Heap(b) => b,
        }
    }

    /// The gram's 64-bit index key: the same rolling polynomial hash that
    /// [`crate::scan`] and [`crate::StreamScanner`] emit for these bytes, so
    /// query-side keys match index-side keys.
    #[must_use]
    pub fn hash(&self) -> u64 {
        hashing::hash_bytes(self.as_bytes())
    }
}

impl From<&[u8]> for Gram {
    #[allow(clippy::indexing_slicing, reason = "bytes.len() <= INLINE_CAP in the inline arm")]
    #[allow(clippy::cast_possible_truncation, reason = "inline arm length <= INLINE_CAP < 256")]
    fn from(bytes: &[u8]) -> Self {
        if bytes.len() <= INLINE_CAP {
            let mut buf = [0u8; INLINE_CAP];
            buf[..bytes.len()].copy_from_slice(bytes);
            Self(Repr::Inline {
                len: bytes.len() as u8,
                buf,
            })
        } else {
            Self(Repr::Heap(bytes.into()))
        }
    }
}

impl Deref for Gram {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl AsRef<[u8]> for Gram {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl Borrow<[u8]> for Gram {
    fn borrow(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl PartialEq for Gram {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl Eq for Gram {}

impl PartialOrd for Gram {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Gram {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.as_bytes().cmp(other.as_bytes())
    }
}

impl Hash for Gram {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // delegate to the slice impl so Borrow<[u8]> lookups stay consistent
        self.as_bytes().hash(state);
    }
}

impl fmt::Debug for Gram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Gram({:?})", String::from_utf8_lossy(self.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footprint_matches_the_vec_it_replaced() {
        assert_eq!(core::mem::size_of::<Gram>(), 24);
    }

    #[test]
    fn inline_and_heap_round_trip() {
        let short = Gram::from(&b"abc"[..]);
        assert_eq!(short.as_bytes(), b"abc");
        let exact = Gram::from(&[7u8; INLINE_CAP][..]);
        assert_eq!(exact.len(), INLINE_CAP);
        let long = Gram::from(&[7u8; INLINE_CAP + 1][..]);
        assert_eq!(long.len(), INLINE_CAP + 1);
        assert_eq!(&long[..INLINE_CAP], &exact[..]);
    }

    #[test]
    fn equality_and_order_are_byte_semantics() {
        let a = Gram::from(&b"abc"[..]);
        let b = Gram::concat(b"ab", b"c");
        assert_eq!(a, b);
        assert!(Gram::from(&b"ab"[..]) < a);
        assert!(Gram::from(&b"abd"[..]) > a);
        // inline/heap boundary must not affect ordering
        let long_a = Gram::from(&[b'a'; 30][..]);
        let long_b = Gram::concat(&[b'a'; 15], &[b'a'; 15]);
        assert_eq!(long_a, long_b);
    }

    #[test]
    fn concat_crossing_the_inline_boundary() {
        let g = Gram::concat(&[b'x'; 12], &[b'y'; 12]);
        assert_eq!(g.len(), 24);
        assert_eq!(&g[..12], &[b'x'; 12]);
        assert_eq!(&g[12..], &[b'y'; 12]);
    }

    #[test]
    fn empty_gram() {
        assert_eq!(Gram::empty().len(), 0);
        assert_eq!(Gram::empty(), Gram::from(&b""[..]));
        assert!(Gram::empty().is_empty());
    }

    #[test]
    fn std_hash_agrees_with_borrowed_slice_lookups() {
        let mut set = std::collections::HashSet::new();
        set.insert(Gram::from(&b"needle"[..]));
        assert!(set.contains(&b"needle"[..]));
    }
}
