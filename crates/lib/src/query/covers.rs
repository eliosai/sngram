//! Covering-gram lookup and OR-of-covers query building.

use core::ops::Range;

use crate::query::gram::Gram;

use crate::scan::cover::Cover;

use super::algebra::{Op, Query};
use super::analyze::{Analyzer, MAX_SET};
use super::order::Order;
use super::settings::QuerySettings;
use super::strings::StringSet;

/// Branch count past which a flushed set covers each string minimally
/// instead of maximally.
const MAX_MAXIMAL_COVER_BRANCHES: usize = 8;

impl Analyzer<'_> {
    /// `q` AND the OR over each string's covering grams. A string shorter
    /// than a gram, or one that covers to nothing, leaves `q` unconstrained,
    /// as does an exhausted [`super::analyze::PLAN_GRAM_BUDGET`] outside the
    /// final flush.
    ///
    /// A flush may spend at most half the remaining budget (floored, see
    /// [`Analyzer::flush_cap`]), measured on the REAL covers: a few-branch
    /// set tries maximal covers first, falls back to each string's minimal
    /// cover, and only then shortens the windows to fit — `order` says which
    /// end to drop, and must match the set's kind so shortening preserves
    /// guaranteed containment (prefix-like sets, including exact sets,
    /// shorten from the tail; suffix sets from the head).
    pub fn and_grams(&self, q: Query, set: &StringSet, order: Order) -> Query {
        if set.is_empty() || set.min_len() < QuerySettings::MIN_GRAM_LEN || !self.may_flush() {
            return q;
        }
        let cap = self.flush_cap();
        if set.len() <= MAX_MAXIMAL_COVER_BRANCHES
            && let Some(covers) = self.maximal_covers(set, cap)
        {
            return self.or_covers(q, covers);
        }
        if let Some(covers) = self.minimal_covers(set, cap) {
            return self.or_covers(q, covers);
        }
        if let Some(grams) = self.branch_single_covers(set, self.budget_left()) {
            let q = self.and_edge_windows(q, set);
            return self.and_or_grams(q, grams);
        }
        self.truncated_cover(q, set, order, cap)
    }

    /// AND in the distinct head and tail windows of an oversized set. The
    /// per-branch single-gram OR is as weak as its most shared member (a
    /// gram missing one class byte admits scattered occurrences), while the
    /// edge windows pin a real seam on each side.
    fn and_edge_windows(&self, q: Query, set: &StringSet) -> Query {
        if set.len() <= MAX_SET {
            return q;
        }
        let q = match distinct_edge(set, Order::Prefix) {
            Some(heads) => self.and_grams(q, &heads, Order::Prefix),
            None => q,
        };
        match distinct_edge(set, Order::Suffix) {
            Some(tails) => self.and_grams(q, &tails, Order::Suffix),
            None => q,
        }
    }

    fn truncated_cover(&self, q: Query, set: &StringSet, order: Order, cap: usize) -> Query {
        let mut fitted = set.clone();
        while fitted.max_len() > QuerySettings::MIN_GRAM_LEN {
            let keep = fitted.max_len() - 1;
            fitted.truncate(order, keep);
            fitted.clean(order);
            if fitted.min_len() < QuerySettings::MIN_GRAM_LEN {
                return q;
            }
            if let Some(covers) = self.minimal_covers(&fitted, cap) {
                return self.or_covers(q, covers);
            }
        }
        q
    }

    /// The maximal covers of each string in `set`, or `None` when a string
    /// covers to nothing or the total exceeds `cap`.
    fn maximal_covers(&self, set: &StringSet, cap: usize) -> Option<Vec<StringSet>> {
        let mut cover = Cover::new(self.table());
        gather_covers(set, cap, |s| {
            let mut grams = StringSet::new();
            cover.each_guaranteed_span(s, |at| grams.push(Gram::from(&s[at])));
            grams.clean(Order::Prefix);
            grams
        })
    }

    /// The minimal covers of each string in `set`, or `None` when a string
    /// covers to nothing or the total exceeds `cap`.
    fn minimal_covers(&self, set: &StringSet, cap: usize) -> Option<Vec<StringSet>> {
        let mut cover = Cover::new(self.table());
        gather_covers(set, cap, |s| {
            let mut grams = StringSet::new();
            cover.each_minimal_span(s, |at| grams.push(Gram::from(&s[at])));
            grams.clean(Order::Prefix);
            grams
        })
    }

    /// One selective gram per string in a large exact/prefix/suffix set.
    ///
    /// Full covers are strongest, but class products such as
    /// `[A-Za-z][A-Za-z]` can have thousands of branches. Truncating those
    /// branches before recording any gram loses the variable-slot
    /// correlation. Keeping the longest guaranteed gram from each branch is
    /// still sound, fits the remaining global budget, and preserves a
    /// correlated middle window for broad finite classes and numeric runs.
    pub fn branch_single_covers(&self, set: &StringSet, cap: usize) -> Option<StringSet> {
        if set.len() > cap {
            return None;
        }
        let mut cover = Cover::new(self.table());
        let mut grams = StringSet::new();
        for s in set.as_slice() {
            grams.push(single_cover(&mut cover, s.as_bytes())?);
        }
        grams.clean(Order::Prefix);
        Some(grams)
    }

    pub fn and_or_grams(&self, q: Query, grams: StringSet) -> Query {
        let spent = grams.len();
        self.spend(spent);
        q.and(Query::grams(Op::Or, grams))
    }

    /// AND into `q` the OR over already-built branch covers, spending the
    /// budget by their exact size.
    fn or_covers(&self, q: Query, covers: Vec<StringSet>) -> Query {
        let mut or = Query::none();
        let mut spent = 0;
        for grams in covers {
            spent += grams.len();
            or = or.or(Query::grams(Op::And, grams));
        }
        self.spend(spent);
        q.and(or)
    }
}

/// Cover every string in `set` with `of`, stopping at the first branch that
/// covers to nothing or takes the total over `cap`.
///
/// A set holding more strings than `cap` is refused unwalked: every string
/// spends at least one gram, so such a walk always ends over the cap, and the
/// single-gram pass would then re-cover the same branches.
///
/// The minimal cover chains a string end to end. The maximal one adds every
/// gram [`crate::scan`] would emit for that string alone: a gram's emission
/// depends only on the bigram weights inside its span, so each of those is
/// emitted for any document containing the string too.
fn gather_covers(
    set: &StringSet,
    cap: usize,
    mut of: impl FnMut(&[u8]) -> StringSet,
) -> Option<Vec<StringSet>> {
    if set.len() > cap {
        return None;
    }
    let mut covers = Vec::with_capacity(set.len());
    let mut total = 0;
    for s in set.as_slice() {
        let grams = of(s.as_bytes());
        total += grams.len();
        if grams.is_empty() || total > cap {
            return None;
        }
        covers.push(grams);
    }
    Some(covers)
}

/// The strongest single guaranteed gram for a branch: longest first, then
/// lexicographic for stable plans. Ranks the cover as it streams, whose order
/// and duplicates cannot move a maximum.
fn single_cover(cover: &mut Cover<'_>, s: &[u8]) -> Option<Gram> {
    let mut best: Option<(bool, Range<usize>)> = None;
    cover.each_guaranteed_span(s, |at| {
        if let Some(spans) = ranks_above(s, best.as_ref(), &at) {
            best = Some((spans, at));
        }
    });
    best.map(|(_, at)| Gram::from(&s[at]))
}

/// Whether the gram at `at` outranks the one kept so far, and the center flag
/// to store with it. Ranking is by center span, then length, then bytes; the
/// center test runs only where it can change the answer.
fn ranks_above(s: &[u8], best: Option<&(bool, Range<usize>)>, at: &Range<usize>) -> Option<bool> {
    let gram = &s[at.clone()];
    let Some((top, kept)) = best else {
        return Some(spans_center(s, at, gram));
    };
    let kept_gram = &s[kept.clone()];
    let longer = (gram.len(), gram) > (kept_gram.len(), kept_gram);
    if *top && !longer {
        return None;
    }
    let spans = spans_center(s, at, gram);
    let above = if spans == *top { longer } else { spans };
    above.then_some(spans)
}

/// True when some occurrence of `gram` in `s` covers the center byte, so a
/// branch's single gram keeps the middle its edge windows cannot pin.
/// The occurrence at `at` is checked first, then the other placements that
/// straddle the center.
fn spans_center(s: &[u8], at: &Range<usize>, gram: &[u8]) -> bool {
    let center = s.len() / 2;
    if at.start <= center && center < at.end {
        return true;
    }
    if gram.is_empty() || gram.len() > s.len() {
        return false;
    }
    let first = center.saturating_sub(gram.len() - 1);
    let last = center.min(s.len() - gram.len());
    (first..=last).any(|start| &s[start..start + gram.len()] == gram)
}

/// The longest edge truncation of `set` that collapses to at most
/// [`MAX_SET`] distinct strings of gram length
fn distinct_edge(set: &StringSet, order: Order) -> Option<StringSet> {
    let mut edge = set.clone();
    edge.clean(order);
    if edge.min_len() < QuerySettings::MIN_GRAM_LEN {
        return None;
    }
    let hi = edge.max_len().saturating_sub(1);
    let keep = edge.longest_fit(order, hi, MAX_SET)?;
    if keep < QuerySettings::MIN_GRAM_LEN {
        return None;
    }
    edge.truncate(order, keep);
    edge.clean(order);
    Some(edge)
}
