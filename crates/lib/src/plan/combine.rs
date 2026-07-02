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

use crate::extract::{self, MIN_LEN};
use crate::gram::Gram;

use super::analyze::{Analyzer, BOUNDARY_GROW, BOUNDARY_KEEP, MAX_EXACT, MAX_EXACT_BYTES, MAX_SET};
use super::info::RegexpInfo;
use super::query::{Op, Query};
use super::strings::{Order, StringSet};

/// Bound on the seam cross product flushed at a concat boundary. Beyond it
/// the boundary strings are truncated back toward codesearch's two-byte
/// stubs, trading precision for a bounded plan.
const MAX_SEAM_CROSS: usize = 128;

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

/// A conservative gram-count estimate for flushing `set`: roughly one gram
/// per bigram of each string under minimal covers.
fn flush_estimate(set: &StringSet) -> usize {
    set.len()
        .saturating_mul(set.max_len().div_ceil(MIN_LEN) + 1)
}

/// Shorten `set`'s strings from the far end until its flush estimate fits
/// `cap`; truncation merges strings that shared the dropped bytes.
fn fit_flush(set: &mut StringSet, order: Order, cap: usize) {
    while flush_estimate(set) > cap && set.max_len() > MIN_LEN {
        truncate_to(set, order, set.max_len() - 1);
        set.clean(order);
    }
}

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
        let over = |a: usize, b: usize| a.saturating_mul(b) > MAX_FLUSH_SET;
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
        self.add_exact(info);
        if let Some(exact) = info.exact.take() {
            spill_exact(info, &exact);
        }
        self.simplify(info, false);
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

    /// `q` AND the OR over each string's covering grams. A string shorter than
    /// a gram, or one that covers to nothing, leaves `q` unconstrained, as
    /// does an exhausted [`super::analyze::PLAN_GRAM_BUDGET`].
    ///
    /// A lone string gets the maximal cover; a many-branch set gets each
    /// string's minimal cover, since the OR over branches dilutes what the
    /// redundant interior grams would add and every gram costs a lookup.
    /// One flush may spend at most half the remaining budget — `order` says
    /// which end of the strings to shorten when it must fit — so the head of
    /// a long pattern can never starve its tail of constraints.
    pub fn and_grams(&self, q: Query, set: &StringSet, order: Order) -> Query {
        if set.is_empty() || set.min_len() < MIN_LEN || !self.within_budget() {
            return q;
        }
        if flush_estimate(set) > self.flush_cap() {
            let mut fitted = set.clone();
            fit_flush(&mut fitted, order, self.flush_cap());
            if fitted.is_empty() || fitted.min_len() < MIN_LEN {
                return q;
            }
            return self.cover_or(q, &fitted);
        }
        self.cover_or(q, set)
    }

    /// AND into `q` the OR over each string's covers, spending the budget.
    fn cover_or(&self, q: Query, set: &StringSet) -> Query {
        let minimal = set.len() > MAX_MAXIMAL_COVER_BRANCHES;
        let mut or = Query::none();
        let mut spent = 0;
        for s in set.as_slice() {
            let grams = if minimal {
                self.minimal_cover_set(s)
            } else {
                self.cover_set(s)
            };
            if grams.is_empty() {
                return q;
            }
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
        let mut set = StringSet::new();
        for gram in extract::cover_one(self.table(), s) {
            set.push(gram);
        }
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
            self.add_exact(info);
            if let Some(exact) = info.exact.take() {
                spill_exact(info, &exact);
            }
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
        if t.len() > MAX_FLUSH_SET {
            // A wide class ballooned the set in one step; covering every
            // string would balloon the plan the same way. Shrink first and
            // cover what survives.
            reduce_set(&mut t, order);
            dedup_redundant(&mut t, order);
        }
        let needs_reduce = t.len() > MAX_SET || t.max_len() > BOUNDARY_GROW;
        if force || needs_reduce {
            let q = mem::replace(&mut info.match_, Query::all());
            info.match_ = self.and_grams(q, &t, order);
        }
        if needs_reduce {
            reduce_set(&mut t, order);
            dedup_redundant(&mut t, order);
        }
        put_set(info, order, t);
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
        return xe.cross(&y.prefix, Order::Prefix);
    }
    let p = x.prefix.clone();
    if x.can_empty {
        return p.union(&y.prefix, Order::Prefix);
    }
    p
}

/// Possible match suffixes of `xy`, where not both sides are exact.
fn concat_suffix(x: &RegexpInfo, y: &RegexpInfo) -> StringSet {
    if let Some(ye) = &y.exact {
        return x.suffix.cross(ye, Order::Suffix);
    }
    let s = y.suffix.clone();
    if y.can_empty {
        return s.union(&x.suffix, Order::Suffix);
    }
    s
}

/// The boundary strings straddling a concat seam, when both sides lack an
/// exact set and the strings are long enough to cover. An oversized cross
/// product is rebuilt from short stubs so the flush stays bounded.
fn seam(x: &RegexpInfo, y: &RegexpInfo) -> Option<StringSet> {
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
