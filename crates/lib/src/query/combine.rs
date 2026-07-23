//! Combining rules for concatenation and alternation.
//!
//! `concat` and `alternate` join two [`RegexpInfo`]s. The structure is
//! Google codesearch's; the bounds and the covering sets are sparse-native:
//! literals cover to every gram [`crate::scan`] would emit for them (the
//! maximal set guaranteed present in any containing document), and windows
//! stay wide instead of degrading to trigrams after the first flush.

use std::mem;

use super::algebra::Query;
use super::analyze::{Analyzer, MAX_EXACT, MAX_SET};
use super::flush::truncate_to;
use super::info::RegexpInfo;
use super::settings::QuerySettings;
use super::strings::{Order, StringSet};

/// Bound on the seam cross product flushed at a concat boundary. Beyond it
/// the boundary strings are truncated back toward codesearch's two-byte
/// stubs, trading precision for a bounded plan.
const MAX_SEAM_CROSS: usize = 2048;

impl Analyzer<'_> {
    /// The summary for `xy` given the summaries of `x` and `y`. Consumes both
    /// match queries rather than cloning them.
    pub fn concat(&self, mut x: RegexpInfo, mut y: RegexpInfo) -> RegexpInfo {
        self.bound_crosses(&mut x, &mut y);
        let mut xy = RegexpInfo::blank();
        // look-around satisfiability reads this flag, so set it faithfully
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

    /// Flush and spill an exact set before a concat whose cross product
    /// would exceed the flush bound, so one wide class cannot balloon a
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
    let left = seam_side(&x.suffix, Order::Suffix)?;
    let right = seam_side(&y.prefix, Order::Prefix)?;
    if left.min_len() + right.min_len() < QuerySettings::MIN_GRAM_LEN {
        return None;
    }
    if left.len().saturating_mul(right.len()) <= MAX_SEAM_CROSS {
        return Some(left.cross(&right, Order::Prefix));
    }
    let (left, right) = shrink_seam(left, right)?;
    if left.min_len() + right.min_len() < QuerySettings::MIN_GRAM_LEN {
        return None;
    }
    Some(left.cross(&right, Order::Prefix))
}

/// A seam side bounded to [`MAX_SET`] strings: an oversized side shrinks to
/// its longest edge truncation instead of dropping the whole seam
fn seam_side(set: &StringSet, order: Order) -> Option<StringSet> {
    if set.len() <= MAX_SET {
        return Some(set.clone());
    }
    let mut edge = set.clone();
    let mut keep = edge.max_len().saturating_sub(1);
    while keep >= 1 {
        truncate_to(&mut edge, order, keep);
        edge.clean(order);
        if edge.len() <= MAX_SET {
            return Some(edge);
        }
        keep -= 1;
    }
    None
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

#[cfg(test)]
mod tests {
    use sngram_types::Gram;

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
    fn seam_side_shrinks_oversized_sets_instead_of_dropping() {
        let mut wide = StringSet::new();
        for a in b'a'..=b'z' {
            for b in b'a'..=b'z' {
                wide.push(Gram::from(&[a, b, b'q'][..]));
            }
        }
        wide.clean(Order::Suffix);
        assert!(wide.len() > MAX_SET);

        let side = seam_side(&wide, Order::Suffix).expect("side should shrink");
        assert!(side.len() <= MAX_SET);
        assert!(side.min_len() >= 1);
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
}
