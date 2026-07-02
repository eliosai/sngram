//! Folding a regex HIR bottom-up into a [`RegexpInfo`].
//!
//! Each HIR node maps to the constructor or combining rule that conservatively
//! describes it. Case-insensitivity needs no handling here: `regex-syntax`
//! expands `(?i)` into character classes during parsing, so concat-of-classes
//! reproduces the folded variant sets for free.

use regex_syntax::hir::{Class, Hir, HirKind, Look, Repetition};

use sngram_types::WeightTable;

use crate::gram::Gram;

use super::info::RegexpInfo;
use super::strings::{Order, StringSet};

/// Flush the exact set once it holds more than this many strings.
///
/// Codesearch used 7 so three case-folded letters (2³ = 8 variants) trigger a
/// flush — all a trigram index can use. Sparse grams keep gaining selectivity
/// with window length, so case-folded windows are allowed to span about six
/// doubling characters before they flush.
pub const MAX_EXACT: usize = 256;
/// Upper bound on prefix and suffix set sizes.
pub const MAX_SET: usize = 128;
/// Upper bound on exact-set bytes retained before spilling into the query.
///
/// Google Code Search spills any exact string longer than a trigram because
/// two bytes of boundary context are enough to recover future trigrams. Sparse
/// grams are variable-length, so retaining exact literals/classes longer lets
/// later concatenation form precise branch-specific covers before we flush.
pub const MAX_EXACT_BYTES: usize = 1024;
/// Bytes a prefix/suffix string may grow to before its window is flushed.
///
/// Codesearch flushed at three bytes (one trigram); wider windows cover to
/// longer, rarer sparse grams before the bytes are dropped.
pub const BOUNDARY_GROW: usize = 15;
/// Bytes of context kept when a prefix/suffix string is truncated after its
/// window flushes, so the next window overlaps the flushed one and adjacency
/// across the cut is not lost. Codesearch kept two.
pub const BOUNDARY_KEEP: usize = 8;
/// Character-class size past which we stop enumerating and over-approximate.
pub const MAX_CLASS: u64 = 100;
/// Copies of a bounded repetition expanded into an explicit concatenation:
/// `x{3}` analyzes as `xxx`, `x{5,}` as `xxx` then `x+`. Beyond this many
/// copies the tail is conservatively folded into the `x+` form.
pub const MAX_REPEAT_EXPAND: u32 = 4;

/// Grams the whole plan may accumulate across every flush. Long case-folded
/// patterns chain hundreds of variant windows whose grams barely overlap;
/// each costs an index lookup, and windows past this budget add almost no
/// selectivity, so further flushes are skipped (each skip only widens the
/// over-approximation).
pub const PLAN_GRAM_BUDGET: usize = 4096;

/// Folds a regex HIR into a conservative gram query using a weight table.
pub struct Analyzer<'a> {
    table: &'a WeightTable,
    /// Gram instances flushed into the plan so far, capped by
    /// [`PLAN_GRAM_BUDGET`].
    flushed: core::cell::Cell<usize>,
}

impl<'a> Analyzer<'a> {
    /// Bind an analyzer to the weight table used to cover literals.
    pub const fn new(table: &'a WeightTable) -> Self {
        Self {
            table,
            flushed: core::cell::Cell::new(0),
        }
    }

    /// The weight table this analyzer covers literals with.
    pub const fn table(&self) -> &WeightTable {
        self.table
    }

    /// Whether the plan still has budget for more covering grams; `spend`
    /// records grams a flush just added.
    pub fn within_budget(&self) -> bool {
        self.flushed.get() < PLAN_GRAM_BUDGET
    }

    /// Record grams a flush added toward [`PLAN_GRAM_BUDGET`].
    pub fn spend(&self, grams: usize) {
        self.flushed.set(self.flushed.get().saturating_add(grams));
    }

    /// Grams the plan may still add before hitting [`PLAN_GRAM_BUDGET`].
    fn budget_left(&self) -> usize {
        PLAN_GRAM_BUDGET.saturating_sub(self.flushed.get())
    }

    /// Most grams one flush may spend: half the remaining budget, floored
    /// so a nearly spent budget still covers something meaningful. The
    /// geometric halving guarantees every region of a long pattern gets a
    /// share instead of the head spending everything.
    pub(super) fn flush_cap(&self) -> usize {
        const MIN_FLUSH_GRAMS: usize = 256;
        (self.budget_left() / 2).max(MIN_FLUSH_GRAMS)
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

    /// Analyze `hir`, returning its conservative summary.
    pub fn analyze(&self, hir: &Hir) -> RegexpInfo {
        let mut info = match hir.kind() {
            HirKind::Empty | HirKind::Look(_) => return RegexpInfo::empty_string(),
            HirKind::Capture(c) => return self.analyze(&c.sub),
            HirKind::Concat(subs) => return self.fold_concat(subs),
            HirKind::Alternation(subs) => return self.fold_alternate(subs),
            HirKind::Repetition(rep) => return self.repetition(rep),
            HirKind::Literal(lit) => RegexpInfo::literal(&lit.0),
            HirKind::Class(cls) => class(cls),
        };
        self.simplify(&mut info, false);
        info
    }

    /// Fold a concatenation, proving look-around assertions against their
    /// byte context. A look sandwiched between subexpressions whose adjacent
    /// bytes make it fail for every combination (like `t\bhe`, or `foo$bar`
    /// with a byte after the anchor) means the whole concatenation matches
    /// nothing.
    fn fold_concat(&self, subs: &[Hir]) -> RegexpInfo {
        let mut acc: Option<RegexpInfo> = None;
        let mut pending: Vec<Look> = Vec::new();
        for sub in subs {
            if let HirKind::Look(look) = sub.kind() {
                pending.push(*look);
                continue;
            }
            let info = self.analyze(sub);
            if looks_blocked(&mut pending, acc.as_ref(), Some(&info)) {
                return RegexpInfo::no_match();
            }
            acc = Some(match acc {
                None => info,
                Some(prev) => self.concat(prev, info),
            });
        }
        if looks_blocked(&mut pending, acc.as_ref(), None) {
            return RegexpInfo::no_match();
        }
        acc.unwrap_or_else(RegexpInfo::empty_string)
    }

    /// `x?`, `x*`, `x+`, `x{n,m}`: expand what is bounded, collapse the rest.
    ///
    /// A small bounded repetition enumerates exactly: `x{2,4}` is
    /// `xx|xxx|xxxx`. An unbounded (or large) one expands its minimum:
    /// `x{n,}` matches `x`ⁿ⁻¹ concatenated with one-or-more `x`, so up to
    /// [`MAX_REPEAT_EXPAND`] leading copies are analyzed as an explicit
    /// concatenation and the rest fold into the `x+` form.
    fn repetition(&self, rep: &Repetition) -> RegexpInfo {
        match (rep.min, rep.max) {
            (min, Some(max)) if max <= MAX_REPEAT_EXPAND => self.enumerate_counts(rep, min, max),
            (0, _) => RegexpInfo::any_match(),
            (min, max) => self.expand_minimum(rep, min, max == Some(min)),
        }
    }

    /// `x{n,m}` with small `m`: the alternation of every allowed count.
    fn enumerate_counts(&self, rep: &Repetition, min: u32, max: u32) -> RegexpInfo {
        let base = self.analyze(&rep.sub);
        let total: u32 = (min..=max).sum();
        if self.affordable_copies(&base, total) < total {
            return if min == 0 {
                RegexpInfo::any_match()
            } else {
                self.expand_from(&base, min, false)
            };
        }
        self.spend(base.match_.weight().saturating_mul(total as usize));
        let mut info: Option<RegexpInfo> = None;
        for k in min..=max {
            let mut power = self.power(&base, k);
            self.flush_sets(&mut power);
            info = Some(match info {
                None => power,
                Some(acc) => self.alternate(acc, power),
            });
        }
        info.unwrap_or_else(RegexpInfo::no_match)
    }

    /// `x{n}`, `x{n,}`, `x{n,m}` with large `m`: expand up to
    /// [`MAX_REPEAT_EXPAND`] leading copies; unless the count is exact and
    /// fully expanded, the last copy folds the rest as one-or-more `x`.
    fn expand_minimum(&self, rep: &Repetition, min: u32, exact_count: bool) -> RegexpInfo {
        let base = self.analyze(&rep.sub);
        self.expand_from(&base, min, exact_count)
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
        let demote_at = if copies == 1 { 1 } else { 2 };
        self.spend(
            base.match_
                .weight()
                .saturating_mul(copies.saturating_sub(1) as usize),
        );
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
        let mut info = info.unwrap_or_else(RegexpInfo::any_match);
        self.simplify(&mut info, false);
        info
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

    /// Left-fold an alternation over its branches.
    ///
    /// Each branch is flushed before the fold unions the prefix/suffix
    /// sets, so its constraints bind inside its own match query: without
    /// this, one branch's prefix and another branch's suffix could jointly
    /// satisfy the merged plan.
    fn fold_alternate(&self, subs: &[Hir]) -> RegexpInfo {
        match subs {
            [] => RegexpInfo::no_match(),
            [one] => self.analyze(one),
            [first, rest @ ..] => {
                let mut info = self.branch(first);
                for sub in rest {
                    let r = self.branch(sub);
                    info = self.alternate(info, r);
                }
                info
            },
        }
    }

    /// Analyze one alternation branch, flushing its sets.
    fn branch(&self, hir: &Hir) -> RegexpInfo {
        let mut info = self.analyze(hir);
        self.flush_sets(&mut info);
        info
    }
}

/// `x+`: at least one `x`, so prefixes and suffixes survive but the whole is
/// no longer an exact set.
fn demote_plus(mut info: RegexpInfo) -> RegexpInfo {
    if let Some(exact) = info.exact.take() {
        info.prefix = exact.clone();
        info.suffix = exact;
    }
    info
}

/// Whether any assertion pending between `left` and `right` is provably
/// unsatisfiable; the pending list is consumed either way. `None` on a side
/// means the pattern edge, where any byte may precede or follow.
fn looks_blocked(
    pending: &mut Vec<Look>,
    left: Option<&RegexpInfo>,
    right: Option<&RegexpInfo>,
) -> bool {
    if pending.is_empty() {
        return false;
    }
    let left = left.and_then(last_bytes);
    let right = right.and_then(first_bytes);
    pending
        .drain(..)
        .any(|look| look_impossible(look, left.as_deref(), right.as_deref()))
}

/// The complete set of bytes a match of `info` can end with, or `None` when
/// unknown. The suffix (or exact) set holds a string every match ends with,
/// so the members' last bytes are exhaustive; an empty-able match has no
/// final byte to speak of.
fn last_bytes(info: &RegexpInfo) -> Option<Vec<u8>> {
    if info.can_empty {
        return None;
    }
    let set = info.exact.as_ref().unwrap_or(&info.suffix);
    boundary_bytes(set.as_slice().iter().map(|s| s.as_bytes().last()))
}

/// The complete set of bytes a match of `info` can start with; see
/// [`last_bytes`].
fn first_bytes(info: &RegexpInfo) -> Option<Vec<u8>> {
    if info.can_empty {
        return None;
    }
    let set = info.exact.as_ref().unwrap_or(&info.prefix);
    boundary_bytes(set.as_slice().iter().map(|s| s.as_bytes().first()))
}

fn boundary_bytes<'a>(members: impl Iterator<Item = Option<&'a u8>>) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    for byte in members {
        bytes.push(*byte?);
    }
    if bytes.is_empty() { None } else { Some(bytes) }
}

/// Whether `look` fails for every combination of adjacent bytes. `None`
/// means the byte on that side is unknown (or absent, at the pattern edge),
/// which keeps the assertion possible. Sound to under-report: a missed
/// impossibility only costs candidates.
fn look_impossible(look: Look, left: Option<&[u8]>, right: Option<&[u8]>) -> bool {
    match look {
        // Text anchors fail whenever any byte provably sits on the wrong
        // side of them.
        Look::Start => left.is_some(),
        Look::End => right.is_some(),
        // Line anchors fail when every adjacent byte is not a terminator.
        Look::StartLF => left.is_some_and(|bytes| bytes.iter().all(|&b| b != b'\n')),
        Look::EndLF => right.is_some_and(|bytes| bytes.iter().all(|&b| b != b'\n')),
        Look::StartCRLF => {
            left.is_some_and(|bytes| bytes.iter().all(|&b| b != b'\n' && b != b'\r'))
        },
        Look::EndCRLF => right.is_some_and(|bytes| bytes.iter().all(|&b| b != b'\n' && b != b'\r')),
        // A word boundary fails when both sides are always the same
        // wordness; its negation fails when they always differ. Non-ASCII
        // bytes have unknown wordness and keep the assertion possible.
        Look::WordAscii | Look::WordUnicode => words_agree(left, right, |l, r| l == r),
        Look::WordAsciiNegate | Look::WordUnicodeNegate => words_agree(left, right, |l, r| l != r),
        _ => false,
    }
}

/// Whether every (left, right) byte pair has ASCII wordness related by
/// `fail`, with both sides known and fully ASCII.
fn words_agree(
    left: Option<&[u8]>,
    right: Option<&[u8]>,
    fail: impl Fn(bool, bool) -> bool,
) -> bool {
    let (Some(left), Some(right)) = (left, right) else {
        return false;
    };
    if left.iter().chain(right).any(|&b| !b.is_ascii()) {
        return false;
    }
    left.iter().all(|&l| {
        right
            .iter()
            .all(|&r| fail(is_word_byte(l), is_word_byte(r)))
    })
}

const fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Describe a character class: empty matches nothing, a wide class is
/// over-approximated as any character, otherwise enumerate its members.
fn class(cls: &Class) -> RegexpInfo {
    match class_set(cls) {
        ClassSet::Empty => RegexpInfo::no_match(),
        ClassSet::Wide => RegexpInfo::any_char(),
        ClassSet::Exact(set) => RegexpInfo {
            exact: Some(set),
            ..RegexpInfo::blank()
        },
    }
}

/// The outcome of enumerating a character class.
enum ClassSet {
    Empty,
    Wide,
    Exact(StringSet),
}

fn class_set(cls: &Class) -> ClassSet {
    match cls {
        Class::Unicode(cu) => {
            let ranges: Vec<(u32, u32)> = cu
                .ranges()
                .iter()
                .map(|r| (r.start() as u32, r.end() as u32))
                .collect();
            enumerate(&ranges, encode_char)
        },
        Class::Bytes(cb) => {
            let ranges: Vec<(u32, u32)> = cb
                .ranges()
                .iter()
                .map(|r| (u32::from(r.start()), u32::from(r.end())))
                .collect();
            enumerate(&ranges, encode_byte)
        },
    }
}

fn enumerate(ranges: &[(u32, u32)], encode: fn(u32) -> Option<Gram>) -> ClassSet {
    let count: u64 = ranges.iter().map(|&(lo, hi)| u64::from(hi - lo) + 1).sum();
    if count == 0 {
        return ClassSet::Empty;
    }
    if count > MAX_CLASS {
        return ClassSet::Wide;
    }
    let mut set = StringSet::new();
    for &(lo, hi) in ranges {
        for c in lo..=hi {
            if let Some(bytes) = encode(c) {
                set.push(bytes);
            }
        }
    }
    if set.is_empty() {
        return ClassSet::Wide;
    }
    set.clean(Order::Prefix);
    ClassSet::Exact(set)
}

fn encode_char(c: u32) -> Option<Gram> {
    let mut buf = [0u8; 4];
    char::from_u32(c).map(|ch| Gram::from(ch.encode_utf8(&mut buf).as_bytes()))
}

fn encode_byte(c: u32) -> Option<Gram> {
    u8::try_from(c).ok().map(|b| Gram::from(&[b][..]))
}
