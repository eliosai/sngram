//! Folding a regex HIR bottom-up into a [`RegexpInfo`].
//!
//! Each HIR node maps to the constructor or combining rule that conservatively
//! describes it. Case-insensitivity needs no handling here: `regex-syntax`
//! expands `(?i)` into character classes during parsing, so concat-of-classes
//! reproduces the folded variant sets for free.

mod boundary;
mod classes;
mod looks;
mod repeat;

use regex_syntax::hir::{Hir, HirKind, Look};

use sngram_types::WeightTable;

use super::algebra::Query;
use super::info::RegexpInfo;

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
            HirKind::Literal(lit) => self.literal_info(&lit.0),
            HirKind::Class(cls) => self.fold_info(classes::class(cls)),
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
            && let Some((exact, wide)) = classes::split_mixed_class(cls)
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
        if looks::looks_blocked(&mut seam_looks, prev.as_mut(), Some(&mut info)) {
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
        if looks::looks_blocked(&mut trailing_looks, Some(&mut info), None) {
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

    /// Whether the scan format brackets documents with line sentinels
    pub const fn line_sentinels(&self) -> bool {
        self.ctx.line_sentinels
    }

    /// A literal's summary, folded when planning in the folded space
    fn literal_info(&self, bytes: &[u8]) -> RegexpInfo {
        if !self.ctx.fold {
            return RegexpInfo::literal(bytes);
        }
        let folded: Vec<u8> = bytes.iter().map(u8::to_ascii_lowercase).collect();
        RegexpInfo::literal(&folded)
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

/// Whether a byte is an ASCII word byte
pub const fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}
