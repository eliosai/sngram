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
/// with window length, so case-folded windows are allowed to span about
/// eight doubling characters before they flush.
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
/// Distinct first- or last-bytes a wide class may keep before that side is
/// dropped to `{""}` (boundary unknown).
///
/// 64 is the count of UTF-8 continuation bytes (`0x80..=0xBF`): every
/// multi-byte scalar ends in one, so a class confined to non-ASCII scalars
/// (like `\p{Greek}`) has at most 64 distinct trailing bytes and this cap
/// keeps its full continuation-byte suffix. A class whose scalars span ASCII
/// too — `.` is `[^\n]`, whose first and last bytes cover almost all of
/// `0x00..=0xFF` — overflows the cap and collapses that side to no
/// constraint, exactly the bare-`any_char` behaviour from before. A 64-way OR
/// of covering windows also stays a small fraction of [`PLAN_GRAM_BUDGET`].
pub const MAX_BOUNDARY_BYTES: usize = 64;
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
    /// Gram instances charged to the plan so far — covering-set flushes
    /// plus repetition-expansion replications — capped by
    /// [`PLAN_GRAM_BUDGET`].
    flushed: core::cell::Cell<usize>,
    /// Whether the final flush is underway: the whole pattern's own edges
    /// flush even on a spent budget (bounded by the flush-cap floor), so a
    /// long pattern's tail is never left entirely unconstrained.
    finalizing: core::cell::Cell<bool>,
}

impl<'a> Analyzer<'a> {
    /// Bind an analyzer to the weight table used to cover literals.
    pub const fn new(table: &'a WeightTable) -> Self {
        Self {
            table,
            flushed: core::cell::Cell::new(0),
            finalizing: core::cell::Cell::new(false),
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

    /// Whether a flush may proceed: within budget, or finalizing.
    pub(super) fn may_flush(&self) -> bool {
        self.within_budget() || self.finalizing.get()
    }

    /// Enter the final flush of the whole pattern's edges.
    pub(crate) fn begin_final_flush(&self) {
        self.finalizing.set(true);
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
            let mut info = self.analyze(sub);
            if looks_blocked(&mut pending, acc.as_mut(), Some(&mut info)) {
                return RegexpInfo::no_match();
            }
            acc = Some(match acc {
                None => info,
                Some(prev) => self.concat(prev, info),
            });
        }
        if looks_blocked(&mut pending, acc.as_mut(), None) {
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
/// unsatisfiable; the pending list is consumed either way. Along the way
/// each assertion FILTERS the adjacent sets: a member whose boundary byte
/// fails the assertion against every byte of the other side cannot occur in
/// a match at this junction, so it drops out — and a set filtered to
/// nothing proves the whole concatenation empty. `None` on a side means the
/// pattern edge, where any byte may precede or follow.
fn looks_blocked(
    pending: &mut Vec<Look>,
    mut left: Option<&mut RegexpInfo>,
    mut right: Option<&mut RegexpInfo>,
) -> bool {
    for look in pending.drain(..) {
        let left_bytes = left.as_deref().and_then(last_bytes);
        let right_bytes = right.as_deref().and_then(first_bytes);
        if let (Some(info), Some(lb)) = (right.as_deref_mut(), &left_bytes) {
            let keep = |b: u8| lb.iter().any(|&l| look_possible(look, Some(l), Some(b)));
            if filter_first_members(info, keep) {
                return true;
            }
        }
        if let (Some(info), Some(rb)) = (left.as_deref_mut(), &right_bytes) {
            let keep = |b: u8| rb.iter().any(|&r| look_possible(look, Some(b), Some(r)));
            if filter_last_members(info, keep) {
                return true;
            }
        }
        if !cross_possible(look, left_bytes.as_deref(), right_bytes.as_deref()) {
            return true;
        }
    }
    false
}

/// Whether `look` can hold for at least one adjacent byte pair; a `None`
/// side stands for every possible byte (or the haystack edge).
fn cross_possible(look: Look, left: Option<&[u8]>, right: Option<&[u8]>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(lb), None) => lb.iter().any(|&l| look_possible(look, Some(l), None)),
        (None, Some(rb)) => rb.iter().any(|&r| look_possible(look, None, Some(r))),
        (Some(lb), Some(rb)) => lb
            .iter()
            .any(|&l| rb.iter().any(|&r| look_possible(look, Some(l), Some(r)))),
    }
}

/// Drop `info`'s exact/prefix members whose FIRST byte fails `keep`;
/// returns true when a known non-empty set filtered to nothing (no match
/// can pass the junction). Empty-string members have no first byte at this
/// junction and always survive.
fn filter_first_members(info: &mut RegexpInfo, keep: impl Fn(u8) -> bool) -> bool {
    let set = match info.exact.as_mut() {
        Some(exact) => exact,
        None => &mut info.prefix,
    };
    if set.is_empty() {
        return false;
    }
    set.retain(|m| m.as_bytes().first().is_none_or(|&b| keep(b)));
    set.is_empty()
}

/// Symmetric to [`filter_first_members`] for exact/suffix LAST bytes.
fn filter_last_members(info: &mut RegexpInfo, keep: impl Fn(u8) -> bool) -> bool {
    let set = match info.exact.as_mut() {
        Some(exact) => exact,
        None => &mut info.suffix,
    };
    if set.is_empty() {
        return false;
    }
    set.retain(|m| m.as_bytes().last().is_none_or(|&b| keep(b)));
    set.is_empty()
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

/// Whether `look` can hold between one concrete byte pair. `None` means
/// the byte is absent (pattern edge next to the haystack boundary) or
/// unknown; unknown keeps the assertion possible. Sound to over-report:
/// a spurious `true` only costs candidates.
fn look_possible(look: Look, left: Option<u8>, right: Option<u8>) -> bool {
    match look {
        // Text anchors: no byte may sit on the anchored side.
        Look::Start => left.is_none(),
        Look::End => right.is_none(),
        // Line anchors: the adjacent byte, if any, must be a terminator.
        Look::StartLF => left.is_none_or(|b| b == b'\n'),
        Look::EndLF => right.is_none_or(|b| b == b'\n'),
        Look::StartCRLF => left.is_none_or(|b| b == b'\n' || b == b'\r'),
        Look::EndCRLF => right.is_none_or(|b| b == b'\n' || b == b'\r'),
        // Word boundaries: ASCII wordness must differ (or agree for the
        // negation). Non-ASCII bytes have unknown wordness; a missing byte
        // is a non-word boundary side.
        Look::WordAscii | Look::WordUnicode => match (word_of(left), word_of(right)) {
            (Some(l), Some(r)) => l != r,
            _ => true,
        },
        Look::WordAsciiNegate | Look::WordUnicodeNegate => match (word_of(left), word_of(right)) {
            (Some(l), Some(r)) => l == r,
            _ => true,
        },
        _ => true,
    }
}

/// The ASCII wordness of an adjacent byte; `None` for unknown — a missing
/// byte (the pattern edge may abut arbitrary haystack context) or a
/// non-ASCII byte.
const fn word_of(byte: Option<u8>) -> Option<bool> {
    match byte {
        Some(b) if b.is_ascii() => Some(is_word_byte(b)),
        _ => None,
    }
}

const fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Describe a character class: empty matches nothing, a wide class keeps its
/// first- and last-byte sets (over-approximating the middle as any character),
/// otherwise enumerate its members.
///
/// A wide class is not collapsed to a bare `any_char`: its `prefix`/`suffix`
/// carry every byte a match can start or end with, as one-byte members. These
/// slot into the ordinary seam and cross machinery — a wide class next to a
/// literal forms `<boundary-byte>literal` windows the plan can cover — while a
/// side whose byte set is too large falls back to `{""}` (boundary unknown),
/// leaving that edge as unconstraining as before.
fn class(cls: &Class) -> RegexpInfo {
    match class_set(cls) {
        ClassSet::Empty => RegexpInfo::no_match(),
        ClassSet::Wide { first, last } => RegexpInfo {
            prefix: first,
            suffix: last,
            ..RegexpInfo::blank()
        },
        ClassSet::Exact(set) => RegexpInfo {
            exact: Some(set),
            ..RegexpInfo::blank()
        },
    }
}

/// The outcome of enumerating a character class.
enum ClassSet {
    Empty,
    /// Over the [`MAX_CLASS`] enumeration cap: the exact set is dropped, but
    /// the exhaustive first-/last-byte sets are kept as one-byte members
    /// (each `{""}` when its side overflowed [`MAX_BOUNDARY_BYTES`]).
    Wide {
        first: StringSet,
        last: StringSet,
    },
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
            enumerate(&ranges, encode_char, utf8_boundary_bytes)
        },
        Class::Bytes(cb) => {
            let ranges: Vec<(u32, u32)> = cb
                .ranges()
                .iter()
                .map(|r| (u32::from(r.start()), u32::from(r.end())))
                .collect();
            enumerate(&ranges, encode_byte, byte_boundary_bytes)
        },
    }
}

/// Derives a wide class's (first-byte, last-byte) boundary sets from its
/// scalar or byte ranges.
type BoundaryFn = fn(&[(u32, u32)]) -> (StringSet, StringSet);

/// Enumerate a class into its exact set, or — once it exceeds [`MAX_CLASS`] —
/// its first/last boundary-byte sets via `boundary`.
fn enumerate(
    ranges: &[(u32, u32)],
    encode: fn(u32) -> Option<Gram>,
    boundary: BoundaryFn,
) -> ClassSet {
    let count: u64 = ranges.iter().map(|&(lo, hi)| u64::from(hi - lo) + 1).sum();
    if count == 0 {
        return ClassSet::Empty;
    }
    if count > MAX_CLASS {
        let (first, last) = boundary(ranges);
        return ClassSet::Wide { first, last };
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
        let (first, last) = boundary(ranges);
        return ClassSet::Wide { first, last };
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

/// First- and last-byte sets of a byte class's members. A byte is its own
/// one-byte "encoding", so both sets are the class's bytes themselves.
fn byte_boundary_bytes(ranges: &[(u32, u32)]) -> (StringSet, StringSet) {
    let mut bytes = ByteSet::new();
    for &(lo, hi) in ranges {
        // Byte-class endpoints are already within 0..=255.
        bytes.mark_range(lo.min(0xFF) as u8, hi.min(0xFF) as u8);
    }
    let set = bytes.into_boundary();
    (set.clone(), set)
}

/// First- and last-byte sets over the UTF-8 encodings of a Unicode class's
/// scalars, derived from the scalar ranges without enumerating each scalar.
///
/// Exhaustiveness (every match starts/ends with a kept byte) holds because
/// both sets are computed as sound supersets: first bytes rise monotonically
/// with the scalar inside one UTF-8 length class, so a sub-range contributes
/// the contiguous span `[first(lo), first(hi)]`; a multi-byte last byte is a
/// continuation byte `0x80 | (cp & 0x3F)`, which cycles through all of
/// `0x80..=0xBF` once a sub-range spans 64 scalars and otherwise walks a short
/// interval. Including a byte no scalar actually produces only costs an unused
/// OR branch; it can never drop a real match.
fn utf8_boundary_bytes(ranges: &[(u32, u32)]) -> (StringSet, StringSet) {
    let mut first = ByteSet::new();
    let mut last = ByteSet::new();
    for &(lo, hi) in ranges {
        mark_utf8_bytes(lo, hi, &mut first, &mut last);
    }
    (first.into_boundary(), last.into_boundary())
}

/// UTF-8 length classes: `(lo, hi, byte_len)` covering all scalar values.
const UTF8_CLASSES: [(u32, u32, u8); 4] = [
    (0x0000, 0x007F, 1),
    (0x0080, 0x07FF, 2),
    (0x0800, 0xFFFF, 3),
    (0x0001_0000, 0x0010_FFFF, 4),
];

/// Mark the first and last UTF-8 bytes of every scalar in `[lo, hi]`.
fn mark_utf8_bytes(lo: u32, hi: u32, first: &mut ByteSet, last: &mut ByteSet) {
    for (clo, chi, len) in UTF8_CLASSES {
        let a = lo.max(clo);
        let b = hi.min(chi);
        if a > b {
            continue;
        }
        // First byte rises monotonically with the scalar inside a length
        // class, so the whole span is covered by its endpoints.
        first.mark_range(utf8_first_byte(a), utf8_first_byte(b));
        if len == 1 {
            // A one-byte scalar's last byte is the scalar itself (ASCII).
            last.mark_range(utf8_last_byte(a), utf8_last_byte(b));
        } else if b - a >= 63 {
            // A span of 64 scalars hits every continuation byte.
            last.mark_range(0x80, 0xBF);
        } else {
            // A short multi-byte span: walk its <= 63 trailing bytes.
            for cp in a..=b {
                last.mark(utf8_last_byte(cp));
            }
        }
    }
}

/// The first byte of `cp`'s UTF-8 encoding, arithmetically (no scalar
/// validity check, so it is safe across the surrogate gap).
#[allow(
    clippy::cast_possible_truncation,
    reason = "each masked value is bounded below 0x100 by construction"
)]
const fn utf8_first_byte(cp: u32) -> u8 {
    if cp < 0x80 {
        cp as u8
    } else if cp < 0x800 {
        0xC0 | (cp >> 6) as u8
    } else if cp < 0x0001_0000 {
        0xE0 | (cp >> 12) as u8
    } else {
        0xF0 | (cp >> 18) as u8
    }
}

/// The last byte of `cp`'s UTF-8 encoding: the scalar itself when ASCII, else
/// the trailing continuation byte.
#[allow(
    clippy::cast_possible_truncation,
    reason = "masked to the low 6 bits, always below 0x100"
)]
const fn utf8_last_byte(cp: u32) -> u8 {
    if cp < 0x80 {
        cp as u8
    } else {
        0x80 | (cp & 0x3F) as u8
    }
}

/// A dense set of bytes, collapsed to a boundary [`StringSet`] once complete.
struct ByteSet([bool; 256]);

impl ByteSet {
    const fn new() -> Self {
        Self([false; 256])
    }

    #[allow(clippy::indexing_slicing, reason = "a u8 index is always < 256")]
    const fn mark(&mut self, b: u8) {
        self.0[b as usize] = true;
    }

    fn mark_range(&mut self, lo: u8, hi: u8) {
        for b in lo..=hi {
            self.mark(b);
        }
    }

    /// The marked bytes as one-byte prefix/suffix members, or `{""}` (boundary
    /// unknown) when none were marked or more than [`MAX_BOUNDARY_BYTES`] were
    /// — the latter as unconstraining as a bare `any_char`.
    fn into_boundary(self) -> StringSet {
        let count = self.0.iter().filter(|&&on| on).count();
        if count == 0 || count > MAX_BOUNDARY_BYTES {
            return StringSet::of(Gram::empty());
        }
        let mut set = StringSet::new();
        for (b, &on) in self.0.iter().enumerate() {
            if on {
                // b came from enumerate(0..256); it fits a u8.
                #[allow(clippy::cast_possible_truncation, reason = "b < 256")]
                set.push(Gram::from(&[b as u8][..]));
            }
        }
        set.clean(Order::Prefix);
        set
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        reason = "tests assert by panicking; UTF-8 encodings are 1..=4 bytes"
    )]

    use regex_syntax::hir::{Class, HirKind};

    use super::{ClassSet, StringSet, class_set, utf8_boundary_bytes};

    /// A boundary set accepts `byte` iff it is a one-byte member, or it is the
    /// `{""}` (unknown) sentinel that accepts anything.
    fn accepts(set: &StringSet, byte: u8) -> bool {
        set.as_slice()
            .iter()
            .any(|g| g.as_bytes().is_empty() || g.as_bytes() == [byte])
    }

    /// The unicode class of `\p{name}`.
    fn script_class(name: &str) -> Class {
        let hir = regex_syntax::parse(&format!("\\p{{{name}}}")).unwrap();
        let HirKind::Class(class) = hir.kind() else {
            panic!("expected a unicode class");
        };
        class.clone()
    }

    /// The scalar ranges of `\p{name}`.
    fn script_ranges(name: &str) -> Vec<(u32, u32)> {
        let Class::Unicode(cu) = script_class(name) else {
            panic!("expected a unicode class");
        };
        cu.ranges()
            .iter()
            .map(|r| (r.start() as u32, r.end() as u32))
            .collect()
    }

    /// Assert `cp`'s actual UTF-8 first and last bytes lie in the boundary sets.
    fn check_scalar(cp: u32, first: &StringSet, last: &StringSet) {
        let Some(ch) = char::from_u32(cp) else {
            return; // surrogate gap: not a scalar
        };
        let mut buf = [0u8; 4];
        let bytes = ch.encode_utf8(&mut buf).as_bytes();
        let (head, tail) = (bytes[0], bytes[bytes.len() - 1]);
        assert!(
            accepts(first, head),
            "first byte {head:#x} of U+{cp:04X} missing"
        );
        assert!(
            accepts(last, tail),
            "last byte {tail:#x} of U+{cp:04X} missing"
        );
    }

    /// Brute-force check: every scalar in `ranges` has its actual UTF-8 first
    /// and last bytes inside the derived boundary sets. This is the
    /// exhaustiveness invariant the plan's soundness rests on.
    fn assert_exhaustive(ranges: &[(u32, u32)]) {
        let (first, last) = utf8_boundary_bytes(ranges);
        for &(lo, hi) in ranges {
            for cp in lo..=hi {
                check_scalar(cp, &first, &last);
            }
        }
    }

    #[test]
    fn utf8_boundary_is_exhaustive_over_scripts() {
        for name in ["Greek", "Cyrillic", "Hebrew", "Han", "Latin"] {
            assert_exhaustive(&script_ranges(name));
        }
    }

    #[test]
    fn utf8_boundary_is_exhaustive_across_length_and_wrap_edges() {
        // Ranges straddling the 1/2/3/4-byte boundaries and wrapping the low
        // six bits, plus the full scalar space.
        assert_exhaustive(&[(0x0000, 0x0010_FFFF)]);
        assert_exhaustive(&[(0x0070, 0x0090)]); // 1->2 byte edge
        assert_exhaustive(&[(0x07F0, 0x0810)]); // 2->3 byte edge
        assert_exhaustive(&[(0xFFF0, 0x0001_0010)]); // 3->4 byte edge
        assert_exhaustive(&[(0x0C3E, 0x0C42), (0x0400, 0x04FF)]); // wrap + block
    }

    #[test]
    fn greek_keeps_a_real_continuation_byte_suffix() {
        // Greek is wide and non-ASCII, so neither side collapses to unknown:
        // its suffix is a genuine continuation-byte constraint (the crux of
        // rejecting a non-Greek `term_var`), and lowercase alpha's trailing
        // byte 0xB1 is among them.
        let ClassSet::Wide { first, last } = class_set(&script_class("Greek")) else {
            panic!("Greek should be a wide class");
        };
        assert!(!first.as_slice().iter().any(|g| g.as_bytes().is_empty()));
        assert!(!last.as_slice().iter().any(|g| g.as_bytes().is_empty()));
        assert!(accepts(&last, 0xB1)); // last byte of U+03B1 (α)
    }
}
