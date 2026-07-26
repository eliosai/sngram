//! Inline small-buffer gram type.
//!
//! Sparse grams are short byte strings in normal use, and the query path was
//! measured allocation-bound: every gram in a `Vec<u8>` was a separate heap
//! box. [`Gram`] stores short values inline and spills longer ones to the heap,
//! eliminating the per-gram allocation for the common case.
//! Representation is canonical (inline iff it fits), so equality and ordering
//! are plain byte comparisons.

use core::borrow::Borrow;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::ops::Deref;

use crate::hashing::HashKey;

struct GramSettings;

impl GramSettings {
    /// Longest gram stored inline; chosen so `size_of::<Gram>() == 24`, the
    /// same footprint as the `Vec<u8>` it replaces.
    const INLINE_CAP: usize = 22;
}

type InlineBuf = [u8; GramSettings::INLINE_CAP];

/// A gram: a short byte string with inline storage.
///
/// Dereferences to `[u8]`; compares, orders, and std-hashes by its bytes.
/// [`Gram::hash`] is the 64-bit index key, identical to the hash emitted for
/// the same bytes by the scanner.
#[derive(Clone)]
pub struct Gram(Repr);

/// Inline buffers are always zero past `len`, so a padded word comparison
/// orders them the same way their bytes do.
#[derive(Clone)]
enum Repr {
    Inline { len: u8, buf: InlineBuf },
    Heap(Box<[u8]>),
}

impl Gram {
    /// The empty gram.
    #[must_use]
    pub const fn empty() -> Self {
        Self::from_inline_parts(0, [0; GramSettings::INLINE_CAP])
    }

    /// Concatenation of two byte strings as one gram, without an intermediate
    /// allocation when the result fits inline.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        reason = "inline arm length <= INLINE_CAP < 256"
    )]
    pub fn concat(a: &[u8], b: &[u8]) -> Self {
        let n = a.len() + b.len();
        if n <= GramSettings::INLINE_CAP {
            let mut buf = Self::empty_inline_buf();
            buf[..a.len()].copy_from_slice(a);
            buf[a.len()..n].copy_from_slice(b);
            Self::from_inline_len(n, buf)
        } else {
            let mut v = Vec::with_capacity(n);
            v.extend_from_slice(a);
            v.extend_from_slice(b);
            Self(Repr::Heap(v.into_boxed_slice()))
        }
    }

    /// The gram's bytes.
    #[must_use]
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        match &self.0 {
            Repr::Inline { len, buf } => &buf[..usize::from(*len)],
            Repr::Heap(b) => b,
        }
    }

    /// The gram's 64-bit index key.
    #[must_use]
    pub fn hash(&self) -> u64 {
        HashKey::UNKEYED.hash_bytes(self.as_bytes())
    }
}

impl Gram {
    const fn from_inline_parts(len: u8, buf: InlineBuf) -> Self {
        Self(Repr::Inline { len, buf })
    }

    const fn empty_inline_buf() -> InlineBuf {
        [0; GramSettings::INLINE_CAP]
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "caller only uses lengths <= INLINE_CAP < 256"
    )]
    const fn from_inline_len(len: usize, buf: InlineBuf) -> Self {
        Self::from_inline_parts(len as u8, buf)
    }

    fn from_inline_bytes(bytes: &[u8]) -> Self {
        let mut buf = Self::empty_inline_buf();
        buf[..bytes.len()].copy_from_slice(bytes);
        Self::from_inline_len(bytes.len(), buf)
    }
}

impl From<&[u8]> for Gram {
    fn from(bytes: &[u8]) -> Self {
        if bytes.len() <= GramSettings::INLINE_CAP {
            Self::from_inline_bytes(bytes)
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
    /// Padding past `len` is always zero, so two inline buffers are equal
    /// exactly when their raw bytes are; byte order never enters equality.
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Repr::Inline { len: a, buf: ab }, Repr::Inline { len: b, buf: bb }) => {
                a == b && ab == bb
            },
            _ => self.as_bytes() == other.as_bytes(),
        }
    }
}

impl Eq for Gram {}

impl PartialOrd for Gram {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Gram {
    #[inline]
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        match (&self.0, &other.0) {
            (Repr::Inline { len: a, buf: ab }, Repr::Inline { len: b, buf: bb }) => {
                words(ab).cmp(&words(bb)).then_with(|| a.cmp(b))
            },
            _ => self.as_bytes().cmp(other.as_bytes()),
        }
    }
}

/// The inline buffer as big-endian words, the integer image whose order
/// matches the padded bytes. The last word overlaps the one before it so the
/// whole buffer is covered by whole loads.
#[inline]
fn words(buf: &InlineBuf) -> [u64; 3] {
    [
        word_at(buf, 0),
        word_at(buf, 8),
        word_at(buf, GramSettings::INLINE_CAP - 8),
    ]
}

#[inline]
fn word_at(buf: &InlineBuf, at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&buf[at..at + 8]);
    u64::from_be_bytes(word)
}

impl Hash for Gram {
    fn hash<H: Hasher>(&self, state: &mut H) {
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
        let exact = Gram::from(&[7u8; GramSettings::INLINE_CAP][..]);
        assert_eq!(exact.len(), GramSettings::INLINE_CAP);
        let long = Gram::from(&[7u8; GramSettings::INLINE_CAP + 1][..]);
        assert_eq!(long.len(), GramSettings::INLINE_CAP + 1);
        assert_eq!(&long[..GramSettings::INLINE_CAP], &exact[..]);
    }

    #[test]
    fn equality_and_order_are_byte_semantics() {
        let a = Gram::from(&b"abc"[..]);
        let b = Gram::concat(b"ab", b"c");
        assert_eq!(a, b);
        assert!(Gram::from(&b"ab"[..]) < a);
        assert!(Gram::from(&b"abd"[..]) > a);
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
    fn concat_exact_inline_boundary_preserves_both_halves() {
        let g = Gram::concat(&[b'a'; 11], &[b'b'; 11]);
        assert_eq!(g.len(), GramSettings::INLINE_CAP);
        assert_eq!(&g[..11], &[b'a'; 11]);
        assert_eq!(&g[11..], &[b'b'; 11]);
    }

    #[test]
    fn empty_gram() {
        assert_eq!(Gram::empty().len(), 0);
        assert_eq!(Gram::empty(), Gram::from(&b""[..]));
        assert!(Gram::empty().is_empty());
    }

    #[test]
    fn packed_order_agrees_with_byte_order() {
        const ALPHABET: [u8; 6] = [0, 1, b'a', b'b', 0x7f, 0xff];
        let mut corpus: Vec<Vec<u8>> = Vec::new();
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..1200 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let len = usize::try_from(state % 26).unwrap_or(0);
            let bytes: Vec<u8> = (0..len)
                .map(|at| ALPHABET[usize::try_from((state >> at) % 6).unwrap_or(0)])
                .collect();
            corpus.push(bytes);
        }
        for a in &corpus {
            for b in &corpus {
                let (ga, gb) = (Gram::from(a.as_slice()), Gram::from(b.as_slice()));
                assert_eq!(ga.cmp(&gb), a.cmp(b), "{a:?} vs {b:?}");
                assert_eq!(ga == gb, a == b, "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn std_hash_agrees_with_borrowed_slice_lookups() {
        let mut set = std::collections::HashSet::new();
        set.insert(Gram::from(&b"needle"[..]));
        assert!(set.contains(&b"needle"[..]));
    }
}
