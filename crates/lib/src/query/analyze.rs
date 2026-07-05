//! Folding a regex HIR bottom-up into a [`RegexpInfo`].
//!
//! Each HIR node maps to the constructor or combining rule that conservatively
//! describes it. Case-insensitivity needs no handling here: `regex-syntax`
//! expands `(?i)` into character classes during parsing, so concat-of-classes
//! reproduces the folded variant sets for free.

use regex_syntax::{
    hir::{Class, Hir, HirKind, Look, Repetition},
    try_is_word_character,
};

use sngram_types::{Gram, WeightTable};

use super::algebra::Query;
use super::info::RegexpInfo;
use super::strings::{Order, StringSet};

/// Flush the exact set once it holds more than this many strings.
///
/// Codesearch used 7 so three case-folded letters (2³ = 8 variants) trigger a
/// flush — all a trigram index can use. Sparse grams keep gaining selectivity
/// with window length, and broad finite classes need to stay correlated across
/// adjacent literals. The byte cap below remains the practical limiter.
pub const MAX_EXACT: usize = 4096;
/// Upper bound on prefix and suffix set sizes.
pub const MAX_SET: usize = 128;
/// Upper bound on exact-set bytes retained before spilling into the query.
///
/// Google Code Search spills any exact string longer than a trigram because
/// two bytes of boundary context are enough to recover future trigrams. Sparse
/// grams are variable-length, so retaining exact literals/classes longer lets
/// later concatenation form precise branch-specific covers before we flush.
/// This admits common two-slot source-code classes such as
/// `[A-Za-z][A-Za-z]` through a following literal while still rejecting larger
/// structured IDs before they can explode.
pub const MAX_EXACT_BYTES: usize = 64 * 1024;
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
/// 128 keeps full boundary sets for mixed source-code alphabets (ASCII
/// letters plus one non-ASCII script) while still collapsing truly arbitrary
/// byte classes such as `.`/`[\x00-\xff]` to the bare-`any_char` behaviour.
/// The resulting OR remains a small fraction of [`PLAN_GRAM_BUDGET`].
pub const MAX_BOUNDARY_BYTES: usize = 128;
/// Copies of a bounded repetition expanded into an explicit concatenation:
/// `x{3}` analyzes as `xxx`, `x{5,}` as `xxx` then `x+`. Beyond this many
/// copies the tail is conservatively folded into the `x+` form.
pub const MAX_REPEAT_EXPAND: u32 = 4;
/// Concat-local alternatives preserved before merging back into one summary.
///
/// Mixed wide classes such as `[A-Za-z\p{Cyrillic}]` need their finite ASCII
/// branch kept separate from the wide Unicode branch until surrounding
/// literals have been crossed in. Otherwise the class's first-byte and
/// last-byte sets can satisfy opposite sides with different one-byte members.
/// The cap keeps repeated mixed classes from turning concat analysis into an
/// exponential expansion; overflow merges branch summaries early, which is a
/// sound precision fallback.
const MAX_CONCAT_ALTERNATIVES: usize = 8;

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
    ctx: PlanContext,
}

/// Index-format facts the analyzer plans against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlanContext {
    /// Plan in the folded gram space: every string byte is ASCII-folded.
    pub fold: bool,
    /// The index brackets documents with virtual `\n` sentinels, so edge
    /// anchors may demand terminator-bridging grams.
    pub line_sentinels: bool,
}

impl<'a> Analyzer<'a> {
    /// Bind an analyzer with explicit index-format context.
    pub const fn with_context(table: &'a WeightTable, ctx: PlanContext) -> Self {
        Self {
            table,
            flushed: core::cell::Cell::new(0),
            finalizing: core::cell::Cell::new(false),
            ctx,
        }
    }

    /// The weight table this analyzer covers literals with.
    pub const fn table(&self) -> &WeightTable {
        self.table
    }

    /// Whether the plan still has budget for more covering grams; `spend`
    /// records grams a flush just added.
    pub const fn within_budget(&self) -> bool {
        self.flushed.get() < PLAN_GRAM_BUDGET
    }

    /// Record grams a flush added toward [`PLAN_GRAM_BUDGET`].
    pub fn spend(&self, grams: usize) {
        self.flushed.set(self.flushed.get().saturating_add(grams));
    }

    /// Whether a flush may proceed: within budget, or finalizing.
    pub const fn may_flush(&self) -> bool {
        self.within_budget() || self.finalizing.get()
    }

    /// Enter the final flush of the whole pattern's edges.
    pub fn begin_final_flush(&self) {
        self.finalizing.set(true);
    }

    /// Grams the plan may still add before hitting [`PLAN_GRAM_BUDGET`].
    pub const fn budget_left(&self) -> usize {
        PLAN_GRAM_BUDGET.saturating_sub(self.flushed.get())
    }

    /// Most grams one flush may spend: half the remaining budget, floored
    /// so a nearly spent budget still covers something meaningful. The
    /// geometric halving guarantees every region of a long pattern gets a
    /// share instead of the head spending everything.
    pub fn flush_cap(&self) -> usize {
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

    /// Analyze and finalize `hir` into the internal gram query.
    pub fn plan(&self, hir: &Hir) -> Query {
        let mut info = self.analyze(hir);
        self.finalize(&mut info);
        info.match_
    }

    fn finalize(&self, info: &mut RegexpInfo) {
        self.begin_final_flush();
        self.simplify(info, true);
        self.add_exact(info);
    }

    /// Analyze `hir`, returning its conservative summary.
    pub fn analyze(&self, hir: &Hir) -> RegexpInfo {
        let mut info = match hir.kind() {
            HirKind::Empty | HirKind::Look(_) => return RegexpInfo::empty_string(),
            HirKind::Capture(c) => return self.analyze(&c.sub),
            HirKind::Concat(subs) => return self.fold_concat(subs),
            HirKind::Alternation(subs) => return self.fold_alternate(subs),
            HirKind::Repetition(rep) => return self.repetition(rep),
            HirKind::Literal(lit) => RegexpInfo::literal(&self.plan_bytes(&lit.0)),
            HirKind::Class(cls) => self.fold_info(class(cls)),
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
        let mut accs: Vec<Option<RegexpInfo>> = vec![None];
        let mut pending: Vec<Look> = Vec::new();
        let (subs, trailing) = self.split_trailing_anchor(subs);
        for sub in subs {
            if let HirKind::Look(look) = sub.kind() {
                self.note_look(&mut pending, &mut accs, *look);
                continue;
            }

            let variants = self.analyze_variants(sub);
            if variants.is_empty() {
                return RegexpInfo::no_match();
            }
            accs = self.concat_variants(accs, &variants, &mut pending);
            if accs.is_empty() {
                return RegexpInfo::no_match();
            }
        }
        let infos = self.finish_concat(accs, trailing.as_ref(), &pending);
        if infos.is_empty() {
            return RegexpInfo::no_match();
        }
        self.merge_infos(infos)
    }

    /// Analyze a concat child, optionally exposing a small set of alternatives
    /// that should stay branch-local until more surrounding context is known.
    fn analyze_variants(&self, hir: &Hir) -> Vec<RegexpInfo> {
        if let HirKind::Class(cls) = hir.kind()
            && let Some((exact, wide)) = split_mixed_class(cls)
        {
            return vec![self.fold_info(exact), self.fold_info(wide)];
        }
        vec![self.analyze(hir)]
    }

    /// Cross the live concat alternatives with a child's branch variants.
    fn concat_variants(
        &self,
        mut accs: Vec<Option<RegexpInfo>>,
        variants: &[RegexpInfo],
        pending: &mut Vec<Look>,
    ) -> Vec<Option<RegexpInfo>> {
        if accs.len().saturating_mul(variants.len()) > MAX_CONCAT_ALTERNATIVES {
            accs = vec![Some(self.merge_accs(accs))];
        }
        let pending_at_seam = pending.clone();
        pending.clear();
        accs.into_iter()
            .flat_map(|acc| self.join_variants(acc.as_ref(), variants, &pending_at_seam))
            .map(Some)
            .collect()
    }

    fn join_variants(
        &self,
        acc: Option<&RegexpInfo>,
        variants: &[RegexpInfo],
        pending: &[Look],
    ) -> Vec<RegexpInfo> {
        variants
            .iter()
            .filter_map(|variant| self.join_variant(acc, variant, pending))
            .collect()
    }

    fn join_variant(
        &self,
        acc: Option<&RegexpInfo>,
        variant: &RegexpInfo,
        pending: &[Look],
    ) -> Option<RegexpInfo> {
        let mut prev = acc.cloned();
        let mut info = variant.clone();
        let mut seam_looks = pending.to_vec();
        if looks_blocked(&mut seam_looks, prev.as_mut(), Some(&mut info)) {
            return None;
        }
        Some(match prev {
            None => info,
            Some(prev) => self.concat(prev, info),
        })
    }

    fn finish_concat(
        &self,
        accs: Vec<Option<RegexpInfo>>,
        trailing: Option<&RegexpInfo>,
        pending: &[Look],
    ) -> Vec<RegexpInfo> {
        accs.into_iter()
            .filter_map(|acc| self.finish_concat_branch(acc, trailing, pending))
            .collect()
    }

    fn finish_concat_branch(
        &self,
        acc: Option<RegexpInfo>,
        trailing: Option<&RegexpInfo>,
        pending: &[Look],
    ) -> Option<RegexpInfo> {
        let mut info = join_trailing(acc, trailing.cloned(), |prev, term| self.concat(prev, term))
            .unwrap_or_else(RegexpInfo::empty_string);
        let mut trailing_looks = pending.to_vec();
        if looks_blocked(&mut trailing_looks, Some(&mut info), None) {
            return None;
        }
        Some(info)
    }

    /// Merge partially built concat alternatives after flushing each branch's
    /// boundary state into its own query, preserving soundness while bounding
    /// the number of live branches.
    fn merge_accs(&self, accs: Vec<Option<RegexpInfo>>) -> RegexpInfo {
        let infos = accs
            .into_iter()
            .map(|acc| acc.unwrap_or_else(RegexpInfo::empty_string))
            .collect();
        self.merge_infos(infos)
    }

    /// Alternation over already-built branch summaries.
    fn merge_infos(&self, mut infos: Vec<RegexpInfo>) -> RegexpInfo {
        let mut info = infos.pop().unwrap_or_else(RegexpInfo::no_match);
        self.flush_sets(&mut info);
        while let Some(mut branch) = infos.pop() {
            self.flush_sets(&mut branch);
            info = self.alternate(branch, info);
        }
        info
    }

    /// A leading start anchor becomes its terminator bytes when the index
    /// carries line sentinels: every `^foo` occurrence in the scanned stream
    /// is preceded by a real or virtual terminator, so the plan may demand
    /// the bridging grams of `\nfoo` — the anchored-literal FP killer.
    /// Anything else stays a pending look for junction pruning
    fn note_look(&self, pending: &mut Vec<Look>, accs: &mut [Option<RegexpInfo>], look: Look) {
        if self.ctx.line_sentinels
            && accs.len() == 1
            && accs.first().is_some_and(Option::is_none)
            && let Some(bytes) = start_terminators(look)
        {
            if let Some(acc) = accs.first_mut() {
                *acc = Some(terminator_info(bytes));
            }
            return;
        }
        pending.push(look);
    }

    /// Split off a trailing end anchor as its terminator info under sentinels
    fn split_trailing_anchor<'h>(&self, subs: &'h [Hir]) -> (&'h [Hir], Option<RegexpInfo>) {
        if !self.ctx.line_sentinels {
            return (subs, None);
        }
        let Some((last, head)) = subs.split_last() else {
            return (subs, None);
        };
        let HirKind::Look(look) = last.kind() else {
            return (subs, None);
        };
        end_terminators(*look).map_or((subs, None), |bytes| (head, Some(terminator_info(bytes))))
    }

    /// ASCII-fold bytes when planning in the folded space
    fn plan_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        if self.ctx.fold {
            bytes.iter().map(u8::to_ascii_lowercase).collect()
        } else {
            bytes.to_vec()
        }
    }

    /// Fold a class node's string state into the folded space
    fn fold_info(&self, mut info: RegexpInfo) -> RegexpInfo {
        if !self.ctx.fold {
            return info;
        }
        if let Some(exact) = info.exact.take() {
            info.exact = Some(exact.fold_ascii());
        }
        info.prefix = info.prefix.fold_ascii();
        info.suffix = info.suffix.fold_ascii();
        info
    }

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
    fn repetition(&self, rep: &Repetition) -> RegexpInfo {
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
            // Closed with a large `m` (or an open tail) over a base too wide to
            // enumerate exactly, and optional: no useful gram survives.
            return RegexpInfo::any_match();
        }
        self.expand_from(&base, min, max == Some(min))
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
        let left_chars = left.as_deref().and_then(last_boundaries);
        let right_chars = right.as_deref().and_then(first_boundaries);
        if let (Some(info), Some(lb)) = (right.as_deref_mut(), &left_chars) {
            let keep = |b: Option<Boundary>| lb.iter().any(|&l| look_possible(look, Some(l), b));
            if filter_first_members(info, keep) {
                return true;
            }
        }
        if let (Some(info), Some(rb)) = (left.as_deref_mut(), &right_chars) {
            let keep = |b: Option<Boundary>| rb.iter().any(|&r| look_possible(look, b, Some(r)));
            if filter_last_members(info, keep) {
                return true;
            }
        }
        if !cross_possible(look, left_chars.as_deref(), right_chars.as_deref()) {
            return true;
        }
    }
    false
}

/// Concat the trailing anchor's terminator info onto the accumulator
fn join_trailing(
    acc: Option<RegexpInfo>,
    trailing: Option<RegexpInfo>,
    concat: impl FnOnce(RegexpInfo, RegexpInfo) -> RegexpInfo,
) -> Option<RegexpInfo> {
    match (acc, trailing) {
        (acc, None) => acc,
        (None, Some(term)) => Some(term),
        (Some(prev), Some(term)) => Some(concat(prev, term)),
    }
}

/// The terminator bytes a start anchor guarantees on its left, under sentinels
const fn start_terminators(look: Look) -> Option<&'static [u8]> {
    match look {
        Look::Start | Look::StartLF => Some(b"\n"),
        Look::StartCRLF => Some(b"\n\r"),
        _ => None,
    }
}

/// The terminator bytes an end anchor guarantees on its right, under sentinels
const fn end_terminators(look: Look) -> Option<&'static [u8]> {
    match look {
        Look::End | Look::EndLF => Some(b"\n"),
        Look::EndCRLF => Some(b"\n\r"),
        _ => None,
    }
}

/// An exact one-byte-per-terminator string set standing in for an anchor
fn terminator_info(bytes: &'static [u8]) -> RegexpInfo {
    let mut set = StringSet::new();
    for &b in bytes {
        set.push(Gram::from(&[b][..]));
    }
    RegexpInfo {
        can_empty: false,
        exact: Some(set),
        prefix: StringSet::new(),
        suffix: StringSet::new(),
        plus_base: None,
        match_: Query::all(),
    }
}

/// Whether `look` can hold for at least one adjacent boundary pair; a `None`
/// side stands for unknown context (or the haystack edge).
fn cross_possible(look: Look, left: Option<&[Boundary]>, right: Option<&[Boundary]>) -> bool {
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
fn filter_first_members(info: &mut RegexpInfo, keep: impl Fn(Option<Boundary>) -> bool) -> bool {
    let set = match info.exact.as_mut() {
        Some(exact) => exact,
        None => &mut info.prefix,
    };
    if set.is_empty() {
        return false;
    }
    set.retain(|m| keep(first_boundary(m.as_bytes())));
    set.is_empty()
}

/// Symmetric to [`filter_first_members`] for exact/suffix LAST bytes.
fn filter_last_members(info: &mut RegexpInfo, keep: impl Fn(Option<Boundary>) -> bool) -> bool {
    let set = match info.exact.as_mut() {
        Some(exact) => exact,
        None => &mut info.suffix,
    };
    if set.is_empty() {
        return false;
    }
    set.retain(|m| keep(last_boundary(m.as_bytes())));
    set.is_empty()
}

/// The complete set of boundary characters a match of `info` can end with,
/// or `None` when unknown. The suffix (or exact) set holds a string every
/// match ends with, so the members' last characters are exhaustive; an
/// empty-able match has no final character to speak of.
fn last_boundaries(info: &RegexpInfo) -> Option<Vec<Boundary>> {
    if info.can_empty {
        return None;
    }
    let set = info.exact.as_ref().unwrap_or(&info.suffix);
    boundaries(set.as_slice().iter().map(|s| last_boundary(s.as_bytes())))
}

/// The complete set of boundary characters a match of `info` can start with;
/// see [`last_boundaries`].
fn first_boundaries(info: &RegexpInfo) -> Option<Vec<Boundary>> {
    if info.can_empty {
        return None;
    }
    let set = info.exact.as_ref().unwrap_or(&info.prefix);
    boundaries(set.as_slice().iter().map(|s| first_boundary(s.as_bytes())))
}

fn boundaries(members: impl Iterator<Item = Option<Boundary>>) -> Option<Vec<Boundary>> {
    let mut out = Vec::new();
    for boundary in members {
        out.push(boundary?);
    }
    if out.is_empty() { None } else { Some(out) }
}

/// The adjacent byte plus any scalar-level wordness proven from a complete
/// UTF-8 character at a regex boundary.
#[derive(Clone, Copy)]
struct Boundary {
    byte: u8,
    ascii_word: bool,
    unicode_word: Option<bool>,
}

fn first_boundary(bytes: &[u8]) -> Option<Boundary> {
    let byte = *bytes.first()?;
    Some(Boundary {
        byte,
        ascii_word: byte.is_ascii() && is_word_byte(byte),
        unicode_word: first_char(bytes).and_then(unicode_word),
    })
}

fn last_boundary(bytes: &[u8]) -> Option<Boundary> {
    let byte = *bytes.last()?;
    Some(Boundary {
        byte,
        ascii_word: byte.is_ascii() && is_word_byte(byte),
        unicode_word: last_char(bytes).and_then(unicode_word),
    })
}

fn first_char(bytes: &[u8]) -> Option<char> {
    let byte = *bytes.first()?;
    let len = utf8_len(byte)?;
    let slice = bytes.get(..len)?;
    let text = core::str::from_utf8(slice).ok()?;
    text.chars().next()
}

fn last_char(bytes: &[u8]) -> Option<char> {
    let mut start = bytes.len().checked_sub(1)?;
    while start > 0
        && bytes
            .get(start)
            .is_some_and(|b| b & 0b1100_0000 == 0b1000_0000)
    {
        start -= 1;
    }
    let slice = bytes.get(start..)?;
    let text = core::str::from_utf8(slice).ok()?;
    let mut chars = text.chars();
    let ch = chars.next()?;
    if chars.next().is_none() {
        Some(ch)
    } else {
        None
    }
}

const fn utf8_len(byte: u8) -> Option<usize> {
    match byte {
        0x00..=0x7F => Some(1),
        0xC2..=0xDF => Some(2),
        0xE0..=0xEF => Some(3),
        0xF0..=0xF4 => Some(4),
        _ => None,
    }
}

fn unicode_word(ch: char) -> Option<bool> {
    try_is_word_character(ch).ok()
}

/// Whether `look` can hold between one concrete boundary pair. `None` means
/// the boundary is absent or unknown; unknown keeps the assertion possible.
/// Sound to over-report: a spurious `true` only costs candidates.
fn look_possible(look: Look, left: Option<Boundary>, right: Option<Boundary>) -> bool {
    match look {
        // Text anchors: no byte may sit on the anchored side.
        Look::Start => left.is_none(),
        Look::End => right.is_none(),
        // Line anchors: the adjacent byte, if any, must be a terminator.
        Look::StartLF => left.is_none_or(|b| b.byte == b'\n'),
        Look::EndLF => right.is_none_or(|b| b.byte == b'\n'),
        Look::StartCRLF => left.is_none_or(|b| b.byte == b'\n' || b.byte == b'\r'),
        Look::EndCRLF => right.is_none_or(|b| b.byte == b'\n' || b.byte == b'\r'),
        // Word boundaries: wordness must differ (or agree for the negation).
        // Unicode wordness is used only when a complete adjacent scalar is
        // known; invalid/truncated UTF-8 stays unknown.
        Look::WordAscii => match (ascii_word_of(left), ascii_word_of(right)) {
            (Some(l), Some(r)) => l != r,
            _ => true,
        },
        Look::WordAsciiNegate => match (ascii_word_of(left), ascii_word_of(right)) {
            (Some(l), Some(r)) => l == r,
            _ => true,
        },
        Look::WordUnicode => match (unicode_word_of(left), unicode_word_of(right)) {
            (Some(l), Some(r)) => l != r,
            _ => true,
        },
        Look::WordUnicodeNegate => match (unicode_word_of(left), unicode_word_of(right)) {
            (Some(l), Some(r)) => l == r,
            _ => true,
        },
        _ => true,
    }
}

const fn ascii_word_of(boundary: Option<Boundary>) -> Option<bool> {
    match boundary {
        Some(b) => Some(b.ascii_word),
        None => None,
    }
}

const fn unicode_word_of(boundary: Option<Boundary>) -> Option<bool> {
    match boundary {
        Some(b) => b.unicode_word,
        None => None,
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
    info_from_class_set(class_set(cls))
}

fn info_from_class_set(set: ClassSet) -> RegexpInfo {
    match set {
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

/// Split a mixed-width Unicode class into a small exact ASCII branch and a
/// residual non-ASCII branch. This keeps the ASCII branch's first/last byte
/// correlation until neighbouring literals are crossed, avoiding a merged
/// wide-class plan that can use different class bytes on each side.
fn split_mixed_class(cls: &Class) -> Option<(RegexpInfo, RegexpInfo)> {
    let ranges = unicode_ranges(cls)?;
    if range_count(&ranges) <= MAX_CLASS {
        return None;
    }
    let (ascii, non_ascii) = partition_ascii_ranges(&ranges);
    let ascii_count = range_count(&ascii);
    if ascii_count == 0 || ascii_count > MAX_CLASS || non_ascii.is_empty() {
        return None;
    }
    let ascii = info_from_class_set(enumerate(&ascii, encode_char, utf8_boundary_bytes));
    let non_ascii = info_from_class_set(enumerate(&non_ascii, encode_char, utf8_boundary_bytes));
    Some((ascii, non_ascii))
}

type ScalarRange = (u32, u32);
type ScalarRanges = Vec<ScalarRange>;

fn unicode_ranges(cls: &Class) -> Option<ScalarRanges> {
    let Class::Unicode(cu) = cls else {
        return None;
    };
    Some(
        cu.ranges()
            .iter()
            .map(|r| (r.start() as u32, r.end() as u32))
            .collect(),
    )
}

fn partition_ascii_ranges(ranges: &[ScalarRange]) -> (ScalarRanges, ScalarRanges) {
    let mut ascii = Vec::new();
    let mut non_ascii = Vec::new();
    for &(lo, hi) in ranges {
        if lo <= 0x7F {
            ascii.push((lo, hi.min(0x7F)));
        }
        if hi >= 0x80 {
            non_ascii.push((lo.max(0x80), hi));
        }
    }
    (ascii, non_ascii)
}

fn range_count(ranges: &[(u32, u32)]) -> u64 {
    ranges.iter().map(|&(lo, hi)| u64::from(hi - lo) + 1).sum()
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
        reason = "tests assert by panicking; UTF-8 encodings are 1..=4 bytes"
    )]

    use regex_syntax::hir::{Class, HirKind, Look};

    use super::{
        ByteSet, ClassSet, MAX_BOUNDARY_BYTES, StringSet, bounded_power_union, class_set,
        first_boundary, last_boundary, look_possible, utf8_boundary_bytes,
    };
    use sngram_types::Gram;

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

    fn set(items: &[&[u8]]) -> StringSet {
        let mut set = StringSet::new();
        for item in items {
            set.push(Gram::from(*item));
        }
        set.clean(super::Order::Prefix);
        set
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
        base.clean(super::Order::Prefix);
        assert!(bounded_power_union(&base, 0, 4).is_none());
    }

    #[test]
    fn overlarge_boundary_byte_set_collapses_to_unknown_member() {
        let mut bytes = ByteSet::new();
        let last = u8::try_from(MAX_BOUNDARY_BYTES).expect("boundary cap fits in u8");
        bytes.mark_range(0, last);
        let set = bytes.into_boundary();
        assert_eq!(set, StringSet::of(Gram::empty()));
    }

    #[test]
    fn byte_set_marks_first_and_last_byte_values() {
        let mut bytes = ByteSet::new();
        bytes.mark(0);
        bytes.mark(u8::MAX);
        let set = bytes.into_boundary();

        assert_eq!(set.len(), 2);
        assert!(accepts(&set, 0));
        assert!(accepts(&set, u8::MAX));
        assert!(!accepts(&set, 1));
    }

    #[test]
    fn unicode_word_boundary_uses_complete_scalars_only() {
        let word_left = last_boundary("α".as_bytes());
        let word_right = first_boundary("β".as_bytes());
        let space_right = first_boundary(b" ");
        let incomplete_right = first_boundary(&[0xCE]);

        assert!(!look_possible(Look::WordUnicode, word_left, word_right));
        assert!(look_possible(Look::WordUnicode, word_left, space_right));
        assert!(
            look_possible(Look::WordUnicode, word_left, incomplete_right),
            "truncated UTF-8 must stay possible instead of proving no match"
        );
    }
}
