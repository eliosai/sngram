//! Bounded simplification: flushing string sets into the match query.
//!
//! `simplify` keeps the exact/prefix/suffix sets and the match query from
//! growing without bound, flushing their grams into the query as they get
//! large.

use std::mem;

use sngram_types::Gram;

use super::algebra::Query;
use super::analyze::{Analyzer, BOUNDARY_GROW, BOUNDARY_KEEP, MAX_EXACT, MAX_EXACT_BYTES, MAX_SET};
use super::info::RegexpInfo;
use super::settings::QuerySettings;
use super::strings::{Order, StringSet};

/// Most strings ever covered in one flush, and the largest exact cross
/// product built in one concat step. Case-folded text doubles per character
/// and stays under this; a wide class (like a hex digit) multiplies by its
/// arity and would otherwise balloon the plan into thousands of OR branches
/// in a single step.
const MAX_FLUSH_SET: usize = 512;

/// After a count-overflow flush, sets shrink to this many strings so several
/// characters of regrowth fit before the next flush: case-folded windows
/// then flush every few characters instead of every character.
const REGROW_TARGET: usize = MAX_SET / 4;

impl Analyzer<'_> {
    /// Add the grams covering `info.exact` into its match query, so they are
    /// not lost when the exact set is later discarded.
    pub fn add_exact(&self, info: &mut RegexpInfo) {
        if let Some(exact) = info.exact.clone() {
            let q = mem::replace(&mut info.match_, Query::all());
            info.match_ = self.and_grams(q, &exact, Order::Prefix);
        }
    }

    /// Cover an exact set into its own match query, then demote it to
    /// prefix/suffix stubs.
    pub fn flush_spill(&self, info: &mut RegexpInfo) {
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
            // spent budget: keep the sets skeletal, no flush can land
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

/// Whether the exact set has grown large enough, or selective enough, to flush.
fn should_flush_exact(info: &RegexpInfo, force: bool) -> bool {
    let Some(exact) = &info.exact else {
        return false;
    };
    let min = exact.min_len();
    exact.len() > MAX_EXACT
        || exact.byte_len() > MAX_EXACT_BYTES
        || (min >= QuerySettings::MIN_GRAM_LEN && force)
}

/// Move each exact string into the prefix and suffix sets as a stub wide
/// enough for the next windows to overlap the flushed one.
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
        // never truncate a non-empty string to the seam-severing "" artifact
        if t.len() <= target || keep <= 1 {
            break;
        }
        keep -= 1;
    }
}

pub fn truncate_to(t: &mut StringSet, order: Order, keep: usize) {
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

    fn set(items: &[&[u8]], order: Order) -> StringSet {
        let mut out = StringSet::new();
        for item in items {
            out.push(Gram::from(*item));
        }
        out.clean(order);
        out
    }

    fn contains_member(set: &StringSet, bytes: &[u8]) -> bool {
        set.as_slice()
            .iter()
            .any(|member| member.as_bytes() == bytes)
    }

    #[test]
    fn spill_exact_keeps_prefix_and_suffix_slices_in_bounds() {
        let exact = set(&[b"", b"a", b"abcdef", b"0123456789abcdef"], Order::Prefix);
        let mut info = RegexpInfo::blank();

        spill_exact(&mut info, &exact);
        info.prefix.clean(Order::Prefix);
        info.suffix.clean(Order::Suffix);

        assert!(
            info.prefix
                .as_slice()
                .iter()
                .all(|member| member.len() <= BOUNDARY_KEEP)
        );
        assert!(
            info.suffix
                .as_slice()
                .iter()
                .all(|member| member.len() <= BOUNDARY_KEEP)
        );
        assert!(contains_member(&info.prefix, b""));
        assert!(contains_member(&info.suffix, b""));
        assert!(contains_member(&info.prefix, b"a"));
        assert!(contains_member(&info.suffix, b"a"));
        assert!(contains_member(&info.prefix, b"01234567"));
        assert!(contains_member(&info.suffix, b"89abcdef"));
    }

    #[test]
    fn truncate_to_respects_prefix_and_suffix_bounds() {
        let mut prefixes = set(&[b"a", b"abcd", b"abcdef"], Order::Prefix);
        truncate_to(&mut prefixes, Order::Prefix, 3);
        prefixes.clean(Order::Prefix);

        assert!(prefixes.as_slice().iter().all(|member| member.len() <= 3));
        assert!(contains_member(&prefixes, b"a"));
        assert!(contains_member(&prefixes, b"abc"));

        let mut suffixes = set(&[b"a", b"abcd", b"zcd", b"abcdef"], Order::Suffix);
        truncate_to(&mut suffixes, Order::Suffix, 3);
        suffixes.clean(Order::Suffix);

        assert!(suffixes.as_slice().iter().all(|member| member.len() <= 3));
        assert!(contains_member(&suffixes, b"a"));
        assert!(contains_member(&suffixes, b"bcd"));
        assert!(contains_member(&suffixes, b"zcd"));
        assert!(contains_member(&suffixes, b"def"));
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
