//! A set of byte strings, the currency of the regex analysis.
//!
//! Ported from Google codesearch `stringSet`. Two sort orders matter: by
//! prefix (ordinary lexicographic) and by suffix (compared from the last
//! byte back). `clean` picks one, then deduplicates, so later passes can
//! merge and truncate prefixes or suffixes in a single linear scan.

/// Sort order for [`StringSet::clean`] and friends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Ordinary lexicographic order; groups shared leading bytes.
    Prefix,
    /// Compared from the last byte back; groups shared trailing bytes.
    Suffix,
}

use crate::gram::Gram;

/// A set of byte strings. Always present; absence is modelled with `Option`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StringSet(Vec<Gram>);

impl StringSet {
    /// The empty set.
    #[must_use]
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// A set holding the single string `s`.
    #[must_use]
    pub fn of(s: Gram) -> Self {
        Self(vec![s])
    }

    /// Append `s` without re-sorting; call [`Self::clean`] before querying.
    pub fn push(&mut self, s: Gram) {
        self.0.push(s);
    }

    /// Keep only the strings `f` accepts, preserving order.
    pub fn retain(&mut self, f: impl FnMut(&Gram) -> bool) {
        self.0.retain(f);
    }

    /// The ASCII-case-folded image of the set, deduplicated.
    #[must_use]
    pub fn fold_ascii(&self) -> Self {
        let mut folded: Vec<Gram> = self
            .0
            .iter()
            .map(|s| {
                let bytes: Vec<u8> = s.as_bytes().iter().map(u8::to_ascii_lowercase).collect();
                Gram::from(bytes.as_slice())
            })
            .collect();
        folded.sort_unstable();
        folded.dedup();
        Self(folded)
    }

    /// Number of strings in the set.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the set is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The strings, in their current order.
    #[must_use]
    pub fn as_slice(&self) -> &[Gram] {
        &self.0
    }

    /// Take ownership of the backing strings.
    #[must_use]
    pub fn into_vec(self) -> Vec<Gram> {
        self.0
    }

    /// Length of the shortest string, or 0 when empty.
    #[must_use]
    pub fn min_len(&self) -> usize {
        self.0.iter().map(|g| g.len()).min().unwrap_or(0)
    }

    /// Length of the longest string, or 0 when empty.
    #[must_use]
    pub fn max_len(&self) -> usize {
        self.0.iter().map(|g| g.len()).max().unwrap_or(0)
    }

    /// Total bytes held by all strings in the set.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.0.iter().map(|g| g.len()).sum()
    }

    /// Sort by `order` and remove duplicates, in place.
    pub fn clean(&mut self, order: Order) {
        match order {
            Order::Prefix => self.0.sort_unstable(),
            Order::Suffix => self
                .0
                .sort_unstable_by(|a, b| cmp_suffix(a.as_bytes(), b.as_bytes())),
        }
        self.0.dedup();
    }

    /// Union with `other`, cleaned in `order`, reusing this set's storage.
    #[must_use]
    pub fn union(mut self, other: &Self, order: Order) -> Self {
        self.0.extend_from_slice(&other.0);
        self.clean(order);
        self
    }

    /// Cross product: every `self` string concatenated with every `other`
    /// string, then cleaned in `order`.
    #[must_use]
    pub fn cross(&self, other: &Self, order: Order) -> Self {
        let mut out = Vec::with_capacity(self.0.len() * other.0.len());
        for a in &self.0 {
            for b in &other.0 {
                out.push(Gram::concat(a.as_bytes(), b.as_bytes()));
            }
        }
        let mut set = Self(out);
        set.clean(order);
        set
    }

    /// Whether every string in `self` also appears in `other`.
    /// Assumes both sets are [`Order::Prefix`]-cleaned.
    #[must_use]
    #[allow(clippy::indexing_slicing, reason = "j guarded by j < other.0.len()")]
    pub fn is_subset_of(&self, other: &Self) -> bool {
        let mut j = 0;
        for s in &self.0 {
            while j < other.0.len() && other.0[j] < *s {
                j += 1;
            }
            if j >= other.0.len() || other.0[j] != *s {
                return false;
            }
        }
        true
    }
}

/// Compare two strings from their last byte back, then by length: the order
/// that groups shared suffixes adjacently for deduplication and truncation.
#[allow(clippy::indexing_slicing, reason = "ia, ib decremented within bounds")]
fn cmp_suffix(a: &[u8], b: &[u8]) -> core::cmp::Ordering {
    let mut ia = a.len();
    let mut ib = b.len();
    while ia > 0 && ib > 0 {
        ia -= 1;
        ib -= 1;
        match a[ia].cmp(&b[ib]) {
            core::cmp::Ordering::Equal => {},
            other => return other,
        }
    }
    a.len().cmp(&b.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&[u8]]) -> StringSet {
        let mut s = StringSet::new();
        for it in items {
            s.push(Gram::from(*it));
        }
        s
    }

    #[test]
    fn test_clean_prefix_sorts_and_dedups() {
        let mut fixture = set(&[b"def", b"abc", b"abc"]);
        fixture.clean(Order::Prefix);
        let expected = set(&[b"abc", b"def"]);
        assert_eq!(fixture, expected);
    }

    #[test]
    fn test_clean_suffix_groups_shared_endings() {
        let mut fixture = set(&[b"xbc", b"abc", b"aac"]);
        fixture.clean(Order::Suffix);
        // All share "bc"/"ac"; suffix order sorts by trailing bytes.
        let expected = set(&[b"aac", b"abc", b"xbc"]);
        assert_eq!(fixture, expected);
    }

    #[test]
    fn test_cross_is_cartesian_concat() {
        let actual = set(&[b"ab", b"cd"]).cross(&set(&[b"x", b"y"]), Order::Prefix);
        let expected = set(&[b"abx", b"aby", b"cdx", b"cdy"]);
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_union_merges_and_cleans() {
        let actual = set(&[b"b", b"a"]).union(&set(&[b"c", b"a"]), Order::Prefix);
        let expected = set(&[b"a", b"b", b"c"]);
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_min_len() {
        let fixture = set(&[b"a", b"abcd", b"ab"]);
        assert_eq!(fixture.min_len(), 1);
    }

    #[test]
    fn test_is_subset_of() {
        let mut sub = set(&[b"a", b"c"]);
        let mut sup = set(&[b"a", b"b", b"c"]);
        sub.clean(Order::Prefix);
        sup.clean(Order::Prefix);
        assert!(sub.is_subset_of(&sup));
        assert!(!sup.is_subset_of(&sub));
    }

    #[test]
    fn test_empty_min_len_is_zero() {
        assert_eq!(StringSet::new().min_len(), 0);
    }
}
