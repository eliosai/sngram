//! Repetition analysis: expansion, enumeration, and demotion.

use regex_syntax::hir::Repetition;

use sngram_types::Gram;

use super::super::info::RegexpInfo;
use super::super::strings::{Order, StringSet};
use super::{Analyzer, MAX_EXACT, MAX_EXACT_BYTES};

/// Copies of a bounded repetition expanded into an explicit concatenation:
/// `x{3}` analyzes as `xxx`, `x{5,}` as `xxx` then `x+`. Beyond this many
/// copies the tail is conservatively folded into the `x+` form.
const MAX_REPEAT_EXPAND: u32 = 4;

impl Analyzer<'_> {
    /// `x?`, `x*`, `x+`, `x{n,m}`: expand what is bounded, collapse the rest.
    ///
    /// When the base analyzes to a small exact set `E` and the closed
    /// repetition's *whole* language stays within [`MAX_EXACT`]/
    /// [`MAX_EXACT_BYTES`], it is expanded to that exact string set even above
    /// [`MAX_REPEAT_EXPAND`] (see [`Self::expand_exact`]): `x{5}` becomes exact
    /// `xxxxx`, `h{3,5}` becomes `{hhh, hhhh, hhhhh}`. The guard is on the
    /// projected set size, not the pattern shape, so a class-heavy base like
    /// `[0-9a-f]{16}` (16¹⁶ strings) folds early exactly as before.
    ///
    /// Otherwise a small `m` enumerates its allowed counts; an unbounded (or
    /// large) minimum expands: `x{n,}` matches `x`ⁿ⁻ᵏ concatenated with `x{k,}`
    /// — the open tail `x{k,}` is `x{k} | x{k+1,}`, so up to
    /// [`MAX_REPEAT_EXPAND`] leading copies are analyzed as an explicit
    /// concatenation and the rest fold into the `x+` form, keeping a full copy
    /// on both edges of the run.
    pub fn repetition(&self, rep: &Repetition) -> RegexpInfo {
        let (min, max) = (rep.min, rep.max);
        if min == 0 && max.is_none() {
            let base = self.analyze(&rep.sub);
            if let Some(info) = demote_star(base) {
                return info;
            }
            return RegexpInfo::any_match(); // `x*`, `x{0,}`
        }
        let base = self.analyze(&rep.sub);
        if let Some(max) = max {
            if let Some(info) = self.expand_exact(&base, min, max) {
                return info;
            }
            if max <= MAX_REPEAT_EXPAND {
                return self.enumerate_counts(&base, min, max);
            }
        }
        if min == 0 {
            // a wide optional repetition retains no useful gram
            return RegexpInfo::any_match();
        }
        self.expand_from(&base, min, max == Some(min))
    }

    /// The most extra copies of `info`'s match query the budget allows.
    /// Expanding a repetition clones the whole accumulated query per copy,
    /// which the flush-side accounting never sees; nested repetitions
    /// otherwise multiply the plan by the copy count at every level.
    fn affordable_copies(&self, info: &RegexpInfo, wanted: u32) -> u32 {
        let weight = info.match_.weight();
        if weight == 0 {
            return wanted;
        }
        let affordable = u32::try_from(self.budget_left() / weight).unwrap_or(u32::MAX);
        wanted.min(affordable.max(1))
    }

    /// Fully expand a *closed* repetition `x{n,m}` whose base is a small exact
    /// set into its exact string language `⋃ₖ₌ₙᵐ Eᵏ`, when that language stays
    /// within [`MAX_EXACT`] strings and [`MAX_EXACT_BYTES`] bytes.
    ///
    /// `Eᵏ` is the `k`-fold cross product of the base's exact set `E`, i.e. the
    /// exact language of `x` repeated `k` times; the union over `k ∈ [n, m]` is
    /// the exact language of `x{n,m}` — no over- or under-approximation, so it
    /// is sound by construction. Returns `None` (falling back to the copy/demote
    /// machinery) for an open repetition, a non-exact base, or a projected set
    /// that would exceed the guard while it is being built.
    ///
    /// The result adopts a blank (`All`) match query, dropping `base.match_`.
    /// That is sound only when the base carries no gram constraint of its own —
    /// which a surviving exact set always satisfies, since flushing an exact set
    /// into the match query is exactly what discards it. The `weight == 0` guard
    /// makes the invariant explicit: a base with real match grams (which cannot
    /// legitimately co-exist with an exact set) falls back to the copy
    /// machinery, where `concat` ANDs `base.match_` into every copy.
    fn expand_exact(&self, base: &RegexpInfo, min: u32, max: u32) -> Option<RegexpInfo> {
        let exact = base.exact.as_ref().filter(|e| !e.is_empty())?;
        if base.match_.weight() != 0 {
            return None;
        }
        let set = bounded_power_union(exact, min, max)?;
        let mut info = RegexpInfo {
            can_empty: set.as_slice().iter().any(|s| s.is_empty()),
            exact: Some(set),
            ..RegexpInfo::blank()
        };
        self.simplify(&mut info, false);
        Some(info)
    }

    /// `x{n,m}` with small `m` and a base too wide to expand exactly: the
    /// alternation of every allowed count.
    fn enumerate_counts(&self, base: &RegexpInfo, min: u32, max: u32) -> RegexpInfo {
        let total: u32 = (min..=max).sum();
        if self.affordable_copies(base, total) < total {
            return if min == 0 {
                RegexpInfo::any_match()
            } else {
                self.expand_from(base, min, false)
            };
        }
        self.spend(base.match_.weight().saturating_mul(total as usize));
        let mut info: Option<RegexpInfo> = None;
        for k in min..=max {
            let mut power = self.power(base, k);
            self.flush_sets(&mut power);
            info = Some(match info {
                None => power,
                Some(acc) => self.alternate(acc, power),
            });
        }
        info.unwrap_or_else(RegexpInfo::no_match)
    }

    /// Expand an already-analyzed base to as many copies as
    /// [`MAX_REPEAT_EXPAND`] and the plan budget afford. Unless the count is
    /// exact and fully expanded, the SECOND copy folds the open end as
    /// one-or-more `x` (`x{n,}` is `x · x{1,} · xⁿ⁻²`), keeping a full copy
    /// on both edges: the leading one extends whatever precedes and the
    /// trailing ones sit adjacent to whatever follows.
    fn expand_from(&self, base: &RegexpInfo, min: u32, exact_count: bool) -> RegexpInfo {
        let copies = self.affordable_copies(base, min.min(MAX_REPEAT_EXPAND));
        let whole = exact_count && copies == min;
        if min == 2 && !whole && copies == 2 {
            return self.split_min_two(base);
        }
        self.spend(
            base.match_
                .weight()
                .saturating_mul(copies.saturating_sub(1) as usize),
        );
        let mut info = self.concat_copies(base, copies, whole);
        self.simplify(&mut info, false);
        info
    }

    /// Concatenate `copies` of `base`, demoting the second (or only) copy
    /// to the one-or-more form unless the count is whole.
    fn concat_copies(&self, base: &RegexpInfo, copies: u32, whole: bool) -> RegexpInfo {
        let demote_at = if copies == 1 { 1 } else { 2 };
        let mut info: Option<RegexpInfo> = None;
        for i in 1..=copies {
            let full_copy = i != demote_at || whole;
            let part = if full_copy {
                base.clone()
            } else {
                demote_plus(base.clone())
            };
            info = Some(match info {
                None => part,
                Some(acc) => self.concat(acc, part),
            });
        }
        info.unwrap_or_else(RegexpInfo::any_match)
    }

    /// `x{2,}` as the exact split `x{2} | x{3,}`: middle demotion needs
    /// three copies to keep two on each edge, so each branch carries two
    /// copies on both edges and seams span the repeat run either way.
    fn split_min_two(&self, base: &RegexpInfo) -> RegexpInfo {
        let two = self.power(base, 2);
        let mut three = self.expand_from(base, 3, false);
        self.flush_sets(&mut three);
        self.alternate(two, three)
    }

    /// `x` concatenated with itself `k` times; zero copies match only the
    /// empty string.
    fn power(&self, base: &RegexpInfo, k: u32) -> RegexpInfo {
        let mut info = RegexpInfo::empty_string();
        for _ in 0..k {
            info = self.concat(info, base.clone());
        }
        info
    }
}

/// `x+`: at least one `x`, so prefixes and suffixes survive but the whole is
/// no longer an exact set.
///
/// When the base is a small exact set `E`, the demoted `E+` records `E` as its
/// [`RegexpInfo::plus_base`] so the enclosing concat can tighten the seam where
/// the run meets a neighbour: `E` alone is an exhaustive prefix/suffix, but an
/// exhaustive OR-set gains nothing from padding it with longer members (the
/// short `E` member still covers every match), so the second boundary byte can
/// only be pinned down at the cross, not in the set. A non-exact base carries
/// no `plus_base`: its language is not a finite set to cross with.
fn demote_plus(mut info: RegexpInfo) -> RegexpInfo {
    if let Some(exact) = info.exact.take() {
        info.prefix = exact.clone();
        info.suffix = exact.clone();
        info.plus_base = Some(exact);
    }
    info
}

/// `x*` over a finite, non-empty exact set keeps bounded edge context while
/// still allowing the empty match through `can_empty`.
fn demote_star(mut info: RegexpInfo) -> Option<RegexpInfo> {
    let mut exact = info.exact.take()?;
    exact.retain(|s| !s.as_bytes().is_empty());
    if exact.is_empty() {
        return None;
    }
    info.can_empty = true;
    info.exact = None;
    info.prefix = exact.clone();
    info.suffix = exact.clone();
    info.plus_base = Some(exact);
    Some(info)
}

/// The exact language `⋃ₖ₌ₘᵢₙᵐᵃˣ Eᵏ` of a closed repetition of the exact set
/// `E`, or `None` if it would exceed [`MAX_EXACT`] strings or
/// [`MAX_EXACT_BYTES`] bytes at any point while it is built.
///
/// `Eᵏ` grows by one cross with `E` per step (`E⁰ = {""}`), and the guard is
/// checked before each growth, so a class-heavy base bails within a few steps
/// (`[0-9a-f]{16}`: `E²` is 256 strings, `E³` overflows) instead of ever
/// materializing the explosion.
fn bounded_power_union(base: &StringSet, min: u32, max: u32) -> Option<StringSet> {
    let mut power = StringSet::of(Gram::empty()); // E⁰ = {""}
    let mut union = StringSet::new();
    for k in 0..=max {
        if k >= min {
            union = union.union(&power, Order::Prefix);
            if union.len() > MAX_EXACT || union.byte_len() > MAX_EXACT_BYTES {
                return None;
            }
        }
        if k == max {
            break;
        }
        power = power.cross(base, Order::Prefix);
        if power.len() > MAX_EXACT || power.byte_len() > MAX_EXACT_BYTES {
            return None;
        }
    }
    Some(union)
}

#[cfg(test)]
mod tests {
    use super::{Gram, Order, StringSet, bounded_power_union};

    fn set(items: &[&[u8]]) -> StringSet {
        let mut set = StringSet::new();
        for item in items {
            set.push(Gram::from(*item));
        }
        set.clean(Order::Prefix);
        set
    }

    #[test]
    fn bounded_power_union_expands_closed_exact_language() {
        let base = set(&[b"ab", b"cd"]);
        let actual = bounded_power_union(&base, 1, 2).expect("small exact language");
        let expected = set(&[b"ab", b"cd", b"abab", b"abcd", b"cdab", b"cdcd"]);
        assert_eq!(actual, expected);
    }

    #[test]
    fn bounded_power_union_rejects_explosive_exact_language() {
        let mut base = StringSet::new();
        for byte in b'a'..=b'q' {
            base.push(Gram::from(&[byte][..]));
        }
        base.clean(Order::Prefix);
        assert!(bounded_power_union(&base, 0, 4).is_none());
    }
}
