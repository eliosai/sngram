//! Combining rules and bounded simplification.
//!
//! `concat` and `alternate` join two [`RegexpInfo`]s; `simplify` keeps the
//! exact/prefix/suffix sets and the match query from growing without bound,
//! flushing their grams into the query as they get large. The structure is
//! Google codesearch's; the bounds and the covering sets are sparse-native:
//! literals cover to every gram [`extract::scan`] would emit for them (the
//! maximal set guaranteed present in any containing document), and windows
//! stay wide instead of degrading to trigrams after the first flush.

use std::mem;

use sngram_types::Gram;

use crate::extract::{self, MIN_LEN};

use super::analyze::{Analyzer, BOUNDARY_GROW, BOUNDARY_KEEP, MAX_EXACT, MAX_EXACT_BYTES, MAX_SET};
use super::info::RegexpInfo;
use super::query::{Op, Query};
use super::strings::{Order, StringSet};

/// Bound on the seam cross product flushed at a concat boundary. Beyond it
/// the boundary strings are truncated back toward codesearch's two-byte
/// stubs, trading precision for a bounded plan.
const MAX_SEAM_CROSS: usize = 2048;

/// Most strings ever covered in one flush, and the largest exact cross
/// product built in one concat step. Case-folded text doubles per character
/// and stays under this; a wide class (like a hex digit) multiplies by its
/// arity and would otherwise balloon the plan into thousands of OR branches
/// in a single step.
const MAX_FLUSH_SET: usize = 512;

/// Branch count past which a flushed set covers each string minimally
/// instead of maximally.
const MAX_MAXIMAL_COVER_BRANCHES: usize = 8;

/// After a count-overflow flush, sets shrink to this many strings so several
/// characters of regrowth fit before the next flush: case-folded windows
/// then flush every few characters instead of every character.
const REGROW_TARGET: usize = MAX_SET / 4;

impl Analyzer<'_> {
    /// The summary for `xy` given the summaries of `x` and `y`. Consumes both
    /// match queries rather than cloning them.
    pub fn concat(&self, mut x: RegexpInfo, mut y: RegexpInfo) -> RegexpInfo {
        self.bound_crosses(&mut x, &mut y);
        let mut xy = RegexpInfo::blank();
        // Codesearch leaves this false and leans on the "" set member; the
        // flag is set faithfully here because look-around satisfiability
        // reads it to know whether a boundary byte must exist.
        xy.can_empty = x.can_empty && y.can_empty;
        let boundary = seam(&x, &y);
        if let (Some(xe), Some(ye)) = (&x.exact, &y.exact) {
            xy.exact = Some(xe.cross(ye, Order::Prefix));
        } else {
            xy.prefix = concat_prefix(&x, &y);
            xy.suffix = concat_suffix(&x, &y);
            apply_plus_hints(&mut xy, &x, &y);
        }
        xy.match_ = x.match_.and(y.match_);
        if let Some(boundary) = boundary {
            let q = mem::replace(&mut xy.match_, Query::all());
            xy.match_ = self.and_grams(q, &boundary, Order::Prefix);
        }
        self.simplify(&mut xy, false);
        xy
    }

    /// The summary for `x|y` given the summaries of `x` and `y`.
    ///
    /// Callers folding a many-branch alternation should flush each branch
    /// with [`Self::flush_sets`] first, so every branch's constraints stay in
    /// its own match query instead of cross-mixing through the unioned
    /// prefix/suffix sets.
    pub fn alternate(&self, mut x: RegexpInfo, mut y: RegexpInfo) -> RegexpInfo {
        let mut xy = RegexpInfo::blank();
        alternate_sets(&mut xy, &x, &y);
        if x.exact.is_some() && y.exact.is_none() {
            self.add_exact(&mut x);
        } else if y.exact.is_some() && x.exact.is_none() {
            self.add_exact(&mut y);
        }
        xy.can_empty = x.can_empty || y.can_empty;
        xy.match_ = x.match_.or(y.match_);
        self.simplify(&mut xy, false);
        xy
    }

    /// Add the grams covering `info.exact` into its match query, so they are
    /// not lost when the exact set is later discarded.
    pub fn add_exact(&self, info: &mut RegexpInfo) {
        if let Some(exact) = info.exact.clone() {
            let q = mem::replace(&mut info.match_, Query::all());
            info.match_ = self.and_grams(q, &exact, Order::Prefix);
        }
    }

    /// Flush and spill an exact set before a concat whose cross product
    /// would exceed [`MAX_FLUSH_SET`], so one wide class cannot balloon a
    /// set (and the flush that follows it) in a single step. The full
    /// windows accumulated so far are covered before any byte is dropped.
    fn bound_crosses(&self, x: &mut RegexpInfo, y: &mut RegexpInfo) {
        let over = |a: usize, b: usize| a.saturating_mul(b) > MAX_EXACT;
        match (&x.exact, &y.exact) {
            (Some(xe), Some(ye)) if over(xe.len(), ye.len()) => self.flush_spill(x),
            (Some(xe), None) if over(xe.len(), y.prefix.len()) => self.flush_spill(x),
            (None, Some(ye)) if over(x.suffix.len(), ye.len()) => self.flush_spill(y),
            _ => {},
        }
    }

    /// Cover an exact set into its own match query, then demote it to
    /// prefix/suffix stubs.
    fn flush_spill(&self, info: &mut RegexpInfo) {
        self.flush_exact(info);
        self.simplify(info, false);
    }

    /// Cover the exact set and spill it into prefix/suffix stubs.
    fn flush_exact(&self, info: &mut RegexpInfo) {
        self.add_exact(info);
        if let Some(exact) = info.exact.take() {
            spill_exact(info, &exact);
        }
    }

    /// Flush a non-exact branch's prefix and suffix covers into its own match
    /// query. Used before alternation unions the sets, which would otherwise
    /// let one branch's prefix satisfy another branch's suffix.
    pub fn flush_sets(&self, info: &mut RegexpInfo) {
        if info.exact.is_some() {
            return;
        }
        let q = mem::replace(&mut info.match_, Query::all());
        let q = self.and_grams(q, &info.prefix, Order::Prefix);
        info.match_ = self.and_grams(q, &info.suffix, Order::Suffix);
    }

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
        if set.is_empty() || set.min_len() < MIN_LEN || !self.may_flush() {
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
            return self.and_or_grams(q, grams);
        }
        self.truncated_cover(q, set, order, cap)
    }

    fn truncated_cover(&self, q: Query, set: &StringSet, order: Order, cap: usize) -> Query {
        let mut fitted = set.clone();
        while fitted.max_len() > MIN_LEN {
            let keep = fitted.max_len() - 1;
            truncate_to(&mut fitted, order, keep);
            fitted.clean(order);
            if fitted.min_len() < MIN_LEN {
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
                self.cover_set(s)
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
    fn branch_single_covers(&self, set: &StringSet, cap: usize) -> Option<StringSet> {
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
        self.cover_set(s).into_vec().into_iter().max_by(|a, b| {
            a.len()
                .cmp(&b.len())
                .then_with(|| a.as_bytes().cmp(b.as_bytes()))
        })
    }

    fn and_or_grams(&self, q: Query, grams: StringSet) -> Query {
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
        for gram in extract::cover_one(self.table(), s) {
            set.push(gram);
        }
        set.clean(Order::Prefix);
        set
    }

    /// Every gram guaranteed to be indexed for a document containing `s`.
    ///
    /// A gram's emission by [`extract::scan`] depends only on the bigram
    /// weights inside its span, so each gram the scan emits for `s` alone is
    /// also emitted when scanning any document that contains `s`. This is the
    /// maximal sound constraint set; the minimal covering set is included for
    /// its equal-weight plateau grams the scan's dedup collapses.
    fn cover_set(&self, s: &[u8]) -> StringSet {
        let mut set = self.minimal_cover_set(s);
        #[allow(
            clippy::indexing_slicing,
            reason = "scan emits start..end spans within s"
        )]
        extract::scan(self.table(), s, |start, end, _| {
            set.push(Gram::from(&s[start..end]));
        });
        set.clean(Order::Prefix);
        set
    }

    /// Bound the exact, prefix, and suffix sets, flushing grams into the match
    /// query as they grow. `force` flushes exact strings that are long enough
    /// to be selective on their own.
    pub fn simplify(&self, info: &mut RegexpInfo, force: bool) {
        if let Some(exact) = &mut info.exact {
            exact.clean(Order::Prefix);
        }
        if should_flush_exact(info, force) {
            self.flush_exact(info);
        }
        if info.exact.is_none() {
            self.simplify_set(info, Order::Prefix, force);
            self.simplify_set(info, Order::Suffix, force);
        }
    }

    /// Bound a prefix or suffix set. The set's covers are flushed into the
    /// match query only when the set must shrink (or at `force`, the final
    /// flush): flushing every step would re-cover the same window each time a
    /// character is appended, while flushing on truncation covers each window
    /// once, exactly before the bytes that formed it are dropped.
    fn simplify_set(&self, info: &mut RegexpInfo, order: Order, force: bool) {
        let mut t = take_set(info, order);
        t.clean(order);
        if !self.may_flush() {
            // Spent budget, not finalizing: no flush can land, so the sets
            // exist only to chain context to the pattern's end for the
            // final flush and the look proofs. Keep them skeletal so the
            // remaining fold costs next to nothing per character.
            reduce_set_skeletal(&mut t, order);
            put_set(info, order, t);
            return;
        }
        let flushed_large_set = self.flush_large_set_before_reduce(info, &mut t, order);
        let needs_reduce = t.len() > MAX_SET || t.max_len() > BOUNDARY_GROW;
        if force || (needs_reduce && !flushed_large_set) {
            let q = mem::replace(&mut info.match_, Query::all());
            info.match_ = self.and_grams(q, &t, order);
        }
        if needs_reduce {
            reduce_set(&mut t, order);
            dedup_redundant(&mut t, order);
        }
        put_set(info, order, t);
    }

    fn flush_large_set_before_reduce(
        &self,
        info: &mut RegexpInfo,
        t: &mut StringSet,
        order: Order,
    ) -> bool {
        if t.len() <= MAX_FLUSH_SET {
            return false;
        }
        let flushed = self
            .branch_single_covers(t, self.budget_left())
            .map(|grams| {
                let q = mem::replace(&mut info.match_, Query::all());
                info.match_ = self.and_or_grams(q, grams);
            })
            .is_some();
        reduce_set(t, order);
        dedup_redundant(t, order);
        flushed
    }
}

/// Fill `xy`'s exact/prefix/suffix from an alternation of `x` and `y`.
fn alternate_sets(xy: &mut RegexpInfo, x: &RegexpInfo, y: &RegexpInfo) {
    match (&x.exact, &y.exact) {
        (Some(xe), Some(ye)) => xy.exact = Some(xe.clone().union(ye, Order::Prefix)),
        (Some(xe), None) => {
            xy.prefix = xe.clone().union(&y.prefix, Order::Prefix);
            xy.suffix = xe.clone().union(&y.suffix, Order::Suffix);
        },
        (None, Some(ye)) => {
            xy.prefix = x.prefix.clone().union(ye, Order::Prefix);
            xy.suffix = x.suffix.clone().union(ye, Order::Suffix);
        },
        (None, None) => {
            xy.prefix = x.prefix.clone().union(&y.prefix, Order::Prefix);
            xy.suffix = x.suffix.clone().union(&y.suffix, Order::Suffix);
        },
    }
}

/// Possible match prefixes of `xy`, where not both sides are exact.
fn concat_prefix(x: &RegexpInfo, y: &RegexpInfo) -> StringSet {
    if let Some(xe) = &x.exact {
        let p = y_prefix(y);
        let crossed = xe.cross(p, Order::Prefix);
        return if y.can_empty {
            crossed.union(xe, Order::Prefix)
        } else {
            crossed
        };
    }
    let p = x.prefix.clone();
    if x.can_empty {
        return p.union(y_prefix(y), Order::Prefix);
    }
    p
}

/// Possible match suffixes of `xy`, where not both sides are exact.
fn concat_suffix(x: &RegexpInfo, y: &RegexpInfo) -> StringSet {
    if let Some(ye) = &y.exact {
        let s = x_suffix(x);
        let crossed = s.cross(ye, Order::Suffix);
        return if x.can_empty {
            crossed.union(ye, Order::Suffix)
        } else {
            crossed
        };
    }
    let s = y.suffix.clone();
    if y.can_empty {
        return s.union(x_suffix(x), Order::Suffix);
    }
    s
}

fn y_prefix(y: &RegexpInfo) -> &StringSet {
    y.exact.as_ref().unwrap_or(&y.prefix)
}

fn x_suffix(x: &RegexpInfo) -> &StringSet {
    x.exact.as_ref().unwrap_or(&x.suffix)
}

/// Tighten the seam where a pure `E+`/`E*` run abuts its neighbour. The
/// result's own `plus_base` stays `None` (blank): the tightened window is
/// baked into `prefix`/`suffix` here, so a later concat crosses it normally.
fn apply_plus_hints(xy: &mut RegexpInfo, x: &RegexpInfo, y: &RegexpInfo) {
    if let Some(e) = y.plus_base.as_ref() {
        xy.suffix = plus_suffix_with_empty_left(x, y, e);
    }
    if let Some(e) = x.plus_base.as_ref() {
        xy.prefix = plus_prefix_with_empty_left(x, y, e);
    }
}

fn plus_suffix_with_empty_left(x: &RegexpInfo, y: &RegexpInfo, e: &StringSet) -> StringSet {
    let suffix = plus_suffix(x, e);
    if y.can_empty {
        suffix.union(x_suffix(x), Order::Suffix)
    } else {
        suffix
    }
}

fn plus_prefix_with_empty_left(x: &RegexpInfo, y: &RegexpInfo, e: &StringSet) -> StringSet {
    let prefix = plus_prefix(y, e);
    if x.can_empty {
        prefix.union(y_prefix(y), Order::Prefix)
    } else {
        prefix
    }
}

/// The exhaustive suffix set of `X·E+` given the left side `x` and the plus
/// base `E`: `(suffix(X) ∪ E) × E`.
///
/// Proof it is exhaustive. A match is `w·e₁···eₖ` with `w ∈ L(X)`, `eᵢ ∈ E`,
/// `k ≥ 1`. Its last two blocks are `w_tail·e₁` when `k = 1` (`w_tail` a suffix
/// of `w`, so in `suffix(X) × E`) or `e_{k-1}·eₖ` when `k ≥ 2` (in `E × E`);
/// either way a member of `(suffix(X) ∪ E) × E`. When `x`'s last byte is
/// unknown (empty `suffix(X)`), fall back to `E` — every match still ends with
/// some `eₖ ∈ E`. The `""` sentinel, if present in `suffix(X)`, contributes
/// `"" × E = E`, the same sound weakening.
fn plus_suffix(x: &RegexpInfo, e: &StringSet) -> StringSet {
    let s = x.exact.as_ref().unwrap_or(&x.suffix);
    if s.is_empty() {
        return e.clone();
    }
    s.clone().union(e, Order::Suffix).cross(e, Order::Suffix)
}

/// The exhaustive prefix set of `E+·Y` given the right side `y` and the plus
/// base `E`: `E × (E ∪ prefix(Y))`. Symmetric to [`plus_suffix`]: the first two
/// blocks of `e₁···eₖ·w` are `e₁·e₂ ∈ E × E` (`k ≥ 2`) or `e₁·w_head ∈
/// E × prefix(Y)` (`k = 1`).
fn plus_prefix(y: &RegexpInfo, e: &StringSet) -> StringSet {
    let p = y.exact.as_ref().unwrap_or(&y.prefix);
    if p.is_empty() {
        return e.clone();
    }
    e.cross(&e.clone().union(p, Order::Prefix), Order::Prefix)
}

/// The boundary strings straddling a concat seam, when both sides lack an
/// exact set and the strings are long enough to cover. An oversized cross
/// product is rebuilt from short stubs so the flush stays bounded.
fn seam(x: &RegexpInfo, y: &RegexpInfo) -> Option<StringSet> {
    if x.can_empty || y.can_empty {
        return None;
    }
    if x.exact.is_some() || y.exact.is_some() {
        return None;
    }
    if x.suffix.len() > MAX_SET || y.prefix.len() > MAX_SET {
        return None;
    }
    if x.suffix.min_len() + y.prefix.min_len() < MIN_LEN {
        return None;
    }
    if x.suffix.len().saturating_mul(y.prefix.len()) <= MAX_SEAM_CROSS {
        return Some(x.suffix.cross(&y.prefix, Order::Prefix));
    }
    let (left, right) = shrink_seam(x.suffix.clone(), y.prefix.clone())?;
    if left.min_len() + right.min_len() < MIN_LEN {
        return None;
    }
    Some(left.cross(&right, Order::Prefix))
}

/// Truncate the larger seam side, one byte at a time, until the cross product
/// fits under [`MAX_SEAM_CROSS`]. Truncation merges strings that shared the
/// dropped byte, shrinking the set.
fn shrink_seam(mut left: StringSet, mut right: StringSet) -> Option<(StringSet, StringSet)> {
    while left.len().saturating_mul(right.len()) > MAX_SEAM_CROSS {
        let (bigger, order) = if left.len() >= right.len() {
            (&mut left, Order::Suffix)
        } else {
            (&mut right, Order::Prefix)
        };
        let keep = bigger.max_len().saturating_sub(1);
        if keep == 0 {
            return None;
        }
        truncate_to(bigger, order, keep);
        bigger.clean(order);
    }
    Some((left, right))
}

/// Whether the exact set has grown large enough, or selective enough, to flush.
fn should_flush_exact(info: &RegexpInfo, force: bool) -> bool {
    let Some(exact) = &info.exact else {
        return false;
    };
    let min = exact.min_len();
    exact.len() > MAX_EXACT || exact.byte_len() > MAX_EXACT_BYTES || (min >= MIN_LEN && force)
}

/// Move each exact string into the prefix and suffix sets as a stub wide
/// enough for the next windows to overlap the flushed one.
#[allow(clippy::indexing_slicing, reason = "k is min(BOUNDARY_KEEP, len)")]
fn spill_exact(info: &mut RegexpInfo, exact: &StringSet) {
    for s in exact.as_slice() {
        let k = BOUNDARY_KEEP.min(s.len());
        info.prefix.push(Gram::from(&s[..k]));
        info.suffix.push(Gram::from(&s[s.len() - k..]));
    }
}

/// Post-budget shrink: a handful of short stubs is all the final flush and
/// the look proofs need, and small sets keep the remaining per-character
/// cross products cheap.
fn reduce_set_skeletal(t: &mut StringSet, order: Order) {
    const SKELETAL_KEEP: usize = BOUNDARY_KEEP / 2;
    const SKELETAL_COUNT: usize = 16;
    let mut keep = SKELETAL_KEEP.min(t.max_len());
    loop {
        truncate_to(t, order, keep);
        t.clean(order);
        if t.len() <= SKELETAL_COUNT || keep <= 1 {
            break;
        }
        keep -= 1;
    }
}

/// Shrink `t` back under its bounds: strings are truncated to
/// [`BOUNDARY_KEEP`] bytes of context, then to ever-shorter prefixes (or
/// suffixes) until an overflowing set drops to [`REGROW_TARGET`] strings,
/// de-duplicating between passes.
fn reduce_set(t: &mut StringSet, order: Order) {
    let target = if t.len() > MAX_SET {
        REGROW_TARGET
    } else {
        MAX_SET
    };
    let mut keep = BOUNDARY_KEEP.min(t.max_len());
    loop {
        truncate_to(t, order, keep);
        t.clean(order);
        // Never truncate non-empty strings to empty: a set of single bytes
        // may overshoot the target (bounded by 256), but an artifact ""
        // would nullify every later cross and sever the seam for good.
        if t.len() <= target || keep <= 1 {
            break;
        }
        keep -= 1;
    }
}

#[allow(clippy::indexing_slicing, reason = "cut only when len > keep")]
fn truncate_to(t: &mut StringSet, order: Order, keep: usize) {
    let items = mem::take(t).into_vec();
    for s in items {
        if s.len() <= keep {
            t.push(s);
        } else if order == Order::Prefix {
            t.push(Gram::from(&s[..keep]));
        } else {
            t.push(Gram::from(&s[s.len() - keep..]));
        }
    }
}

/// Drop strings made redundant by a shorter one already in the set: if `ab`
/// is a possible prefix, `abc` adds nothing.
fn dedup_redundant(t: &mut StringSet, order: Order) {
    let items = mem::take(t).into_vec();
    let mut kept: Vec<Gram> = Vec::new();
    for s in items {
        let covered = kept.last().is_some_and(|p| match order {
            Order::Prefix => s.starts_with(p.as_bytes()),
            Order::Suffix => s.ends_with(p.as_bytes()),
        });
        if !covered {
            kept.push(s);
        }
    }
    for s in kept {
        t.push(s);
    }
}

fn take_set(info: &mut RegexpInfo, order: Order) -> StringSet {
    match order {
        Order::Prefix => mem::take(&mut info.prefix),
        Order::Suffix => mem::take(&mut info.suffix),
    }
}

fn put_set(info: &mut RegexpInfo, order: Order, set: StringSet) {
    match order {
        Order::Prefix => info.prefix = set,
        Order::Suffix => info.suffix = set,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered_set(prefix: u8, count: usize) -> StringSet {
        let mut out = StringSet::new();
        for number in 0..count {
            out.push(Gram::from(
                format!("{}{:02}", prefix as char, number).as_bytes(),
            ));
        }
        out.clean(Order::Prefix);
        out
    }

    #[test]
    fn shrink_seam_bounds_large_cross_product_without_emptying_context() {
        let left = numbered_set(b'l', 64);
        let right = numbered_set(b'r', 64);
        let (left, right) = shrink_seam(left, right).expect("seam should fit");

        assert!(left.len().saturating_mul(right.len()) <= MAX_SEAM_CROSS);
        assert!(left.min_len() > 0);
        assert!(right.min_len() > 0);
    }

    #[test]
    fn reduce_set_never_introduces_empty_unknown_member() {
        let mut wide = StringSet::new();
        for byte in 0u8..=199 {
            wide.push(Gram::from(&[byte][..]));
        }
        wide.clean(Order::Prefix);

        reduce_set(&mut wide, Order::Prefix);

        assert!(
            wide.as_slice()
                .iter()
                .all(|member| !member.as_bytes().is_empty())
        );
    }
}
