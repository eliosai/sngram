//! A set of byte strings, the currency of the regex analysis.
//!
//! Ported from Google codesearch `stringSet`. `clean` sorts in one of the two
//! [`Order`]s and deduplicates, so later passes can merge and truncate
//! prefixes or suffixes in a single linear scan.
//!
//! A set remembers the order it was last sorted in. The window-shrinking
//! loops truncate the same set to ever-shorter strings, and truncation is
//! monotone in both orders, so those passes re-sort nothing.

use sngram_types::Gram;

use super::order::{Order, Shape, cmp_in};

/// A set of byte strings. Always present; absence is modelled with `Option`.
#[derive(Debug, Clone, Default)]
pub struct StringSet {
    items: Vec<Gram>,
    shape: Shape,
}

impl PartialEq for StringSet {
    fn eq(&self, other: &Self) -> bool {
        self.items == other.items
    }
}

impl Eq for StringSet {}

impl StringSet {
    /// The empty set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            items: Vec::new(),
            shape: Shape::Loose,
        }
    }

    /// A set holding the single string `s`.
    #[must_use]
    pub fn of(s: Gram) -> Self {
        Self {
            items: vec![s],
            shape: Shape::Loose,
        }
    }

    /// Append `s` without re-sorting; call [`Self::clean`] before querying.
    pub fn push(&mut self, s: Gram) {
        self.items.push(s);
        self.shape = Shape::Loose;
    }

    /// Keep only the strings `f` accepts, preserving order.
    pub fn retain(&mut self, f: impl FnMut(&Gram) -> bool) {
        self.items.retain(f);
    }

    /// The ASCII-case-folded image of the set, deduplicated.
    #[must_use]
    pub fn fold_ascii(&self) -> Self {
        let mut folded: Vec<Gram> = self
            .items
            .iter()
            .map(|s| {
                let bytes: Vec<u8> = s.as_bytes().iter().map(u8::to_ascii_lowercase).collect();
                Gram::from(bytes.as_slice())
            })
            .collect();
        folded.sort_unstable();
        folded.dedup();
        Self {
            items: folded,
            shape: Shape::Clean(Order::Prefix),
        }
    }

    /// Number of strings in the set.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the set is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The strings, in their current order.
    #[must_use]
    pub fn as_slice(&self) -> &[Gram] {
        &self.items
    }

    /// Take ownership of the backing strings.
    #[must_use]
    pub fn into_vec(self) -> Vec<Gram> {
        self.items
    }

    /// Length of the shortest string, or 0 when empty.
    #[must_use]
    pub fn min_len(&self) -> usize {
        self.items.iter().map(|g| g.len()).min().unwrap_or(0)
    }

    /// Length of the longest string, or 0 when empty.
    #[must_use]
    pub fn max_len(&self) -> usize {
        self.items.iter().map(|g| g.len()).max().unwrap_or(0)
    }

    /// Total bytes held by all strings in the set.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.items.iter().map(|g| g.len()).sum()
    }

    /// Sort by `order` and remove duplicates, in place. A set already known
    /// to hold that shape does no work.
    pub fn clean(&mut self, order: Order) {
        if self.shape.clean_in(order) {
            return;
        }
        if !self.shape.sorted_in(order) {
            match order {
                Order::Prefix => self.items.sort_unstable(),
                Order::Suffix => self.items.sort_unstable_by(|a, b| cmp_in(order, a, b)),
            }
        }
        self.items.dedup();
        self.shape = Shape::Clean(order);
    }

    /// Shorten every string to its leading (or trailing) `keep` bytes, the
    /// end `order` names. Both orders compare the kept bytes before anything
    /// the cut drops, so a set sorted in `order` stays sorted, though the
    /// cut can make neighbours equal.
    pub fn truncate(&mut self, order: Order, keep: usize) {
        for s in &mut self.items {
            if s.len() <= keep {
                continue;
            }
            *s = match order {
                Order::Prefix => Gram::from(&s[..keep]),
                Order::Suffix => Gram::from(&s[s.len() - keep..]),
            };
        }
        self.shape = if self.shape.sorted_in(order) {
            Shape::Sorted(order)
        } else {
            Shape::Loose
        };
    }

    /// Union with `other`, cleaned in `order`, reusing this set's storage.
    /// Two sets already in `order` merge instead of re-sorting.
    #[must_use]
    pub fn union(mut self, other: &Self, order: Order) -> Self {
        if self.shape.sorted_in(order) && other.shape.sorted_in(order) {
            self.merge_from(&other.items, order);
            self.shape = Shape::Sorted(order);
        } else {
            self.items.extend_from_slice(&other.items);
            self.shape = Shape::Loose;
        }
        self.clean(order);
        self
    }

    /// Merge an already-ordered `other` into this already-ordered set,
    /// filling the grown tail from the back so no second buffer is needed.
    fn merge_from(&mut self, other: &[Gram], order: Order) {
        let held = self.items.len();
        self.items.resize(held + other.len(), Gram::empty());
        let (mut i, mut j, mut k) = (held, other.len(), held + other.len());
        while j > 0 {
            k -= 1;
            if i > 0 && cmp_in(order, &self.items[i - 1], &other[j - 1]).is_gt() {
                i -= 1;
                self.items.swap(k, i);
            } else {
                j -= 1;
                self.items[k] = other[j].clone();
            }
        }
    }

    /// Whether `s` is a member. Assumes [`Order::Prefix`] order, walking only
    /// the run that could hold `s` when the order was never recorded.
    #[must_use]
    pub fn contains(&self, s: &Gram) -> bool {
        if self.shape.sorted_in(Order::Prefix) {
            let at = self.items.partition_point(|item| item < s);
            return self.items.get(at).is_some_and(|item| item == s);
        }
        self.items
            .iter()
            .take_while(|item| *item <= s)
            .any(|item| item == s)
    }

    /// Cross product: every `self` string concatenated with every `other`
    /// string, then cleaned in `order`.
    ///
    /// Crossing a fixed-width left side onto a clean right side emits the
    /// pairs already in lexicographic order, so the common class-product
    /// case sorts nothing.
    #[must_use]
    pub fn cross(&self, other: &Self, order: Order) -> Self {
        let mut out = Vec::with_capacity(self.items.len() * other.items.len());
        for a in &self.items {
            for b in &other.items {
                out.push(Gram::concat(a.as_bytes(), b.as_bytes()));
            }
        }
        let mut set = Self {
            items: out,
            shape: self.cross_shape(other),
        };
        set.clean(order);
        set
    }

    /// The size [`Self::cross`] with `other` would reach, when a fixed width
    /// on this side makes every concatenation distinct so no pair collapses.
    /// `None` when the count cannot be known without building it.
    #[must_use]
    pub fn cross_len(&self, other: &Self) -> Option<usize> {
        let countable = self.shape.is_clean() && other.shape.is_clean() && self.uniform_len();
        countable.then(|| self.items.len().saturating_mul(other.items.len()))
    }

    /// The shape a cross product inherits from its operands.
    fn cross_shape(&self, other: &Self) -> Shape {
        let ordered = self.shape.clean_in(Order::Prefix)
            && other.shape.clean_in(Order::Prefix)
            && self.uniform_len();
        if ordered {
            Shape::Clean(Order::Prefix)
        } else {
            Shape::Loose
        }
    }

    /// Whether every string is the same length, so concatenation cannot
    /// reorder the pairs it builds.
    fn uniform_len(&self) -> bool {
        let mut lens = self.items.iter().map(|g| g.len());
        let Some(first) = lens.next() else {
            return true;
        };
        lens.all(|len| len == first)
    }

    /// Whether every string in `self` also appears in `other`.
    /// Assumes both sides are in [`Order::Prefix`].
    #[must_use]
    pub fn is_subset_of(&self, other: &[Gram]) -> bool {
        let mut j = 0;
        for s in &self.items {
            while j < other.len() && other[j] < *s {
                j += 1;
            }
            if j >= other.len() || other[j] != *s {
                return false;
            }
        }
        true
    }
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
        // all share "bc"/"ac"; suffix order sorts by trailing bytes
        let expected = set(&[b"aac", b"abc", b"xbc"]);
        assert_eq!(fixture, expected);
    }

    fn cleaned(items: &[&[u8]], order: Order) -> StringSet {
        let mut out = set(items);
        out.clean(order);
        out
    }

    /// Sorting from scratch, the shape shortcuts must agree with.
    fn resorted(items: &[Gram], order: Order) -> Vec<Gram> {
        let mut out = items.to_vec();
        out.sort_by(|a, b| cmp_in(order, a, b));
        out.dedup();
        out
    }

    #[test]
    fn shape_shortcuts_match_a_full_resort() {
        for order in [Order::Prefix, Order::Suffix] {
            let left = cleaned(&[b"aq", b"bq", b"zz", b"q"], order);
            let right = cleaned(&[b"aq", b"cq", b"a"], order);
            let mut joint = left.as_slice().to_vec();
            joint.extend_from_slice(right.as_slice());
            assert_eq!(
                left.clone().union(&right, order).as_slice(),
                resorted(&joint, order)
            );
            let crossed = left.cross(&right, order);
            assert_eq!(crossed.as_slice(), resorted(crossed.as_slice(), order));
        }
        let fixed = cleaned(&[b"aa", b"ab", b"zz"], Order::Prefix);
        let ragged = cleaned(&[b"a", b"aa"], Order::Prefix);
        assert_eq!(fixed.cross_len(&ragged), Some(6));
        // "a"+"aa" and "aa"+"a" collide, so no count can be trusted up front
        assert_eq!(ragged.cross_len(&ragged), None);
        assert_eq!(ragged.cross(&ragged, Order::Prefix).len(), 3);
    }

    #[test]
    fn contains_agrees_with_a_scan_of_the_members() {
        let mut ordered = cleaned(&[b"aa", b"bb", b"cc"], Order::Prefix);
        for probe in [&b"aa"[..], b"bb", b"cc", b"ab", b"", b"zz"] {
            let probe = Gram::from(probe);
            let scanned = ordered.as_slice().contains(&probe);
            assert_eq!(ordered.contains(&probe), scanned, "{probe:?}");
        }
        // the recorded order is what lets the search skip ahead
        ordered.push(Gram::from(&b"aa"[..]));
        assert!(ordered.contains(&Gram::from(&b"aa"[..])));
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
        assert_eq!(set(&[b"a", b"abcd", b"ab"]).min_len(), 1);
        assert_eq!(StringSet::new().min_len(), 0);
    }

    #[test]
    fn test_is_subset_of() {
        let empty = StringSet::new();
        let sub = cleaned(&[b"a", b"c"], Order::Prefix);
        let tail = cleaned(&[b"z"], Order::Prefix);
        let sup = cleaned(&[b"a", b"b", b"c", b"z"], Order::Prefix);

        assert!(sub.is_subset_of(sup.as_slice()));
        assert!(!sup.is_subset_of(sub.as_slice()));
        assert!(empty.is_subset_of(empty.as_slice()));
        assert!(empty.is_subset_of(sup.as_slice()));
        assert!(tail.is_subset_of(sup.as_slice()));
        assert!(!sup.is_subset_of(tail.as_slice()));
    }

    #[test]
    fn test_clean_suffix_handles_empty_and_single_byte_members() {
        let fixture = cleaned(&[b"ba", b"", b"a", b"ca", b"a"], Order::Suffix);
        let expected = set(&[b"", b"a", b"ba", b"ca"]);
        assert_eq!(fixture, expected);
    }
}
