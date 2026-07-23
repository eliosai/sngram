//! Covering-gram lookup and OR-of-covers query building.

use sngram_types::Gram;

use crate::scan;

use super::algebra::{Op, Query};
use super::analyze::{Analyzer, MAX_SET};
use super::flush::truncate_to;
use super::settings::QuerySettings;
use super::strings::{Order, StringSet};

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
            && let Some(covers) = self.branch_covers(set, false, cap)
        {
            return self.or_covers(q, covers);
        }
        if let Some(covers) = self.branch_covers(set, true, cap) {
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
            truncate_to(&mut fitted, order, keep);
            fitted.clean(order);
            if fitted.min_len() < QuerySettings::MIN_GRAM_LEN {
                return q;
            }
            if let Some(covers) = self.branch_covers(&fitted, true, cap) {
                return self.or_covers(q, covers);
            }
        }
        q
    }

    /// The covers of each string in `set` — maximal or minimal — or `None`
    /// when a string covers to nothing or the total exceeds `cap`.
    fn branch_covers(&self, set: &StringSet, minimal: bool, cap: usize) -> Option<Vec<StringSet>> {
        let mut covers = Vec::with_capacity(set.len());
        let mut total = 0;
        for s in set.as_slice() {
            let grams = if minimal {
                self.minimal_cover_set(s)
            } else {
                self.guaranteed_cover_set(s)
            };
            if grams.is_empty() {
                return None;
            }
            total += grams.len();
            if total > cap {
                return None;
            }
            covers.push(grams);
        }
        Some(covers)
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
        let mut grams = StringSet::new();
        for s in set.as_slice() {
            grams.push(self.single_cover(s.as_bytes())?);
        }
        grams.clean(Order::Prefix);
        Some(grams)
    }

    /// The strongest single guaranteed gram for a branch: longest first,
    /// then lexicographic for stable plans.
    fn single_cover(&self, s: &[u8]) -> Option<Gram> {
        self.guaranteed_cover_set(s)
            .into_vec()
            .into_iter()
            .max_by(|a, b| {
                spans_center(s, a)
                    .cmp(&spans_center(s, b))
                    .then_with(|| a.len().cmp(&b.len()))
                    .then_with(|| a.as_bytes().cmp(b.as_bytes()))
            })
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

    /// The minimal covering grams of `s`, chaining it end to end.
    fn minimal_cover_set(&self, s: &[u8]) -> StringSet {
        let mut set = StringSet::new();
        for gram in scan::cover::minimal_cover(self.table(), s) {
            set.push(gram);
        }
        set.clean(Order::Prefix);
        set
    }

    /// Every gram guaranteed to be indexed for a document containing `s`.
    ///
    /// A gram's emission by [`crate::scan`] depends only on the bigram
    /// weights inside its span, so each gram the scan emits for `s` alone is
    /// also emitted when scanning any document that contains `s`. This is the
    /// maximal sound constraint set; the minimal covering set is included for
    /// its equal-weight plateau grams the scan's dedup collapses.
    fn guaranteed_cover_set(&self, s: &[u8]) -> StringSet {
        let mut set = StringSet::new();
        for gram in scan::cover::guaranteed_cover(self.table(), s) {
            set.push(gram);
        }
        set.clean(Order::Prefix);
        set
    }
}

/// True when some occurrence of `gram` in `s` covers the center byte, so a
/// branch's single gram keeps the middle its edge windows cannot pin
fn spans_center(s: &[u8], gram: &Gram) -> bool {
    let center = s.len() / 2;
    let gram = gram.as_bytes();
    s.windows(gram.len())
        .enumerate()
        .any(|(at, window)| window == gram && at <= center && center < at + gram.len())
}

/// The longest edge truncation of `set` that collapses to at most
/// [`MAX_SET`] distinct strings of gram length
fn distinct_edge(set: &StringSet, order: Order) -> Option<StringSet> {
    let mut keep = set.max_len().saturating_sub(1);
    while keep >= QuerySettings::MIN_GRAM_LEN {
        let mut edge = set.clone();
        truncate_to(&mut edge, order, keep);
        edge.clean(order);
        if edge.min_len() < QuerySettings::MIN_GRAM_LEN {
            return None;
        }
        if edge.len() <= MAX_SET {
            return Some(edge);
        }
        keep -= 1;
    }
    None
}
