//! A set of byte strings, the currency of the regex analysis.
//!
//! Ported from Google codesearch `stringSet`. `clean` sorts in one of the two
//! [`Order`]s and deduplicates, so later passes can merge and truncate
//! prefixes or suffixes in a single linear scan.
//!
//! A set remembers the order it was last sorted in, and truncation is monotone
//! in both orders, so shortening a sorted set re-sorts nothing.

use crate::query::gram::Gram;

use super::order::{Order, Shape, cmp_in, shared_in};

mod ops;

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

    /// The longest `keep` in `1..=hi` whose [`Self::truncate`] leaves at most
    /// `target` distinct strings, or `None` when even one byte leaves more.
    ///
    /// Two neighbours in `order` collapse under a `keep`-byte truncation
    /// exactly when `keep` is at most the bytes they share at that end, so one
    /// pass over the neighbours prices every `keep` at once. The set must be
    /// cleaned in `order`.
    #[must_use]
    pub fn longest_fit(&self, order: Order, hi: usize, target: usize) -> Option<usize> {
        if hi == 0 {
            return None;
        }
        if self.items.len() <= target {
            return Some(hi);
        }
        let mut cuts = vec![0usize; hi];
        for pair in self.items.windows(2) {
            let shared = shared_in(order, &pair[0], &pair[1]);
            if shared < hi {
                cuts[shared] += 1;
            }
        }
        let mut distinct = 1;
        for (keep, split) in cuts.iter().enumerate() {
            distinct += split;
            if distinct > target {
                return (keep > 0).then_some(keep);
            }
        }
        Some(hi)
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

    /// The shrinking loop `longest_fit` replaced: cut one byte at a time
    /// until the set fits, reporting the last length that did.
    fn shrink_to_fit(t: &StringSet, order: Order, hi: usize, target: usize) -> Option<usize> {
        let mut cut = t.clone();
        for keep in (1..=hi).rev() {
            cut.truncate(order, keep);
            cut.clean(order);
            if cut.len() <= target {
                return Some(keep);
            }
        }
        None
    }

    /// A spread of sets whose truncations collapse at different lengths.
    fn fit_corpus() -> Vec<StringSet> {
        let mut corpus = vec![StringSet::new(), set(&[b""]), set(&[b"abcdef"])];
        let mut wide = StringSet::new();
        let mut ragged = StringSet::new();
        for a in b'a'..=b'f' {
            for b in b'a'..=b'f' {
                wide.push(Gram::from(&[a, b, b'q', a][..]));
                ragged.push(Gram::from(&[a; 1][..]));
                ragged.push(Gram::from(&[a, b, b'z'][..]));
            }
        }
        corpus.push(wide);
        corpus.push(ragged);
        corpus
    }

    /// Every bound one set is asked for agrees with the shrinking loop.
    fn check_fits(t: &StringSet, order: Order) {
        for hi in 0..6 {
            for target in 1..8 {
                assert_eq!(
                    t.longest_fit(order, hi, target),
                    shrink_to_fit(t, order, hi, target),
                    "{order:?} hi {hi} target {target} {:?}",
                    t.as_slice()
                );
            }
        }
    }

    #[test]
    fn longest_fit_matches_shrinking_one_byte_at_a_time() {
        for order in [Order::Prefix, Order::Suffix] {
            for mut t in fit_corpus() {
                t.clean(order);
                check_fits(&t, order);
            }
        }
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
