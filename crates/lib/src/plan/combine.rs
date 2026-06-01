//! Combining rules and bounded simplification.
//!
//! `concat` and `alternate` join two [`RegexpInfo`]s; `simplify` keeps the
//! exact/prefix/suffix sets and the match query from growing without bound,
//! flushing their grams into the query as they get large. Ported 1:1 from
//! Google codesearch, with sparse `cover_one` in place of trigram extraction.

use std::mem;

use crate::extract::{self, MIN_LEN};

use super::analyze::{Analyzer, BOUNDARY_CTX, MAX_EXACT, MAX_SET};
use super::info::RegexpInfo;
use super::query::{Op, Query};
use super::strings::{Order, StringSet};

impl Analyzer<'_> {
    /// The summary for `xy` given the summaries of `x` and `y`. Consumes both
    /// match queries rather than cloning them.
    pub fn concat(&self, x: RegexpInfo, y: RegexpInfo) -> RegexpInfo {
        let mut xy = RegexpInfo::blank();
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
            xy.match_ = self.and_grams(q, &boundary);
        }
        self.simplify(&mut xy, false);
        xy
    }

    /// The summary for `x|y` given the summaries of `x` and `y`.
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
            info.match_ = self.and_grams(q, &exact);
        }
    }

    /// `q` AND the OR over each string's covering grams. A string shorter than
    /// a gram, or one that covers to nothing, leaves `q` unconstrained.
    pub fn and_grams(&self, q: Query, set: &StringSet) -> Query {
        if set.min_len() < MIN_LEN {
            return q;
        }
        let mut or = Query::none();
        for s in set.as_slice() {
            let grams = self.cover_set(s);
            if grams.is_empty() {
                return q;
            }
            or = or.or(Query::grams(Op::And, grams));
        }
        q.and(or)
    }

    fn cover_set(&self, s: &[u8]) -> StringSet {
        let mut set = StringSet::new();
        for gram in extract::cover_one(self.table(), s) {
            set.push(gram);
        }
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
            self.simplify_set(info, Order::Prefix);
            self.simplify_set(info, Order::Suffix);
        }
    }

    /// Flush a prefix or suffix set's grams into the match query, then shrink
    /// the set back under [`MAX_SET`] by truncating and de-duplicating.
    fn simplify_set(&self, info: &mut RegexpInfo, order: Order) {
        let mut t = take_set(info, order);
        t.clean(order);
        let q = mem::replace(&mut info.match_, Query::all());
        info.match_ = self.and_grams(q, &t);
        reduce_set(&mut t, order);
        dedup_redundant(&mut t, order);
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
        }
        (None, Some(ye)) => {
            xy.prefix = x.prefix.clone().union(ye, Order::Prefix);
            xy.suffix = x.suffix.clone().union(ye, Order::Suffix);
        }
        (None, None) => {
            xy.prefix = x.prefix.clone().union(&y.prefix, Order::Prefix);
            xy.suffix = x.suffix.clone().union(&y.suffix, Order::Suffix);
        }
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
/// exact set, the sets are small, and the strings are long enough to cover.
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
    Some(x.suffix.cross(&y.prefix, Order::Prefix))
}

/// Whether the exact set has grown large enough, or selective enough, to flush.
fn should_flush_exact(info: &RegexpInfo, force: bool) -> bool {
    let Some(exact) = &info.exact else {
        return false;
    };
    let min = exact.min_len();
    exact.len() > MAX_EXACT || (min >= MIN_LEN && force) || min > MIN_LEN
}

/// Move each exact string into the prefix and suffix sets as a short stub.
#[allow(clippy::indexing_slicing, reason = "k is min(BOUNDARY_CTX, len)")]
fn spill_exact(info: &mut RegexpInfo, exact: &StringSet) {
    for s in exact.as_slice() {
        if s.len() < MIN_LEN {
            info.prefix.push(s.clone());
            info.suffix.push(s.clone());
        } else {
            let k = BOUNDARY_CTX.min(s.len());
            info.prefix.push(s[..k].to_vec());
            info.suffix.push(s[s.len() - k..].to_vec());
        }
    }
}

/// Shrink `t` under [`MAX_SET`] by truncating its strings to ever-shorter
/// prefixes (or suffixes), de-duplicating after each pass. Strings up to
/// [`BOUNDARY_CTX`] bytes are kept intact while the set stays small enough.
fn reduce_set(t: &mut StringSet, order: Order) {
    let mut keep = BOUNDARY_CTX.min(t.max_len());
    loop {
        truncate_to(t, order, keep);
        t.clean(order);
        if t.len() <= MAX_SET || keep == 0 {
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
            t.push(s[..keep].to_vec());
        } else {
            t.push(s[s.len() - keep..].to_vec());
        }
    }
}

/// Drop strings made redundant by a shorter one already in the set: if `ab`
/// is a possible prefix, `abc` adds nothing.
fn dedup_redundant(t: &mut StringSet, order: Order) {
    let items = mem::take(t).into_vec();
    let mut kept: Vec<Vec<u8>> = Vec::new();
    for s in items {
        let covered = kept.last().is_some_and(|p| match order {
            Order::Prefix => s.starts_with(p),
            Order::Suffix => s.ends_with(p),
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
