//! Complete execution of public sparse-query plans against an eg index.

use std::{collections::HashMap, rc::Rc};

use sngram_types::{DfStats, GramKey, GramNeedle, PlanExpr, QueryPlan, ScanNeed};

use super::summary::{SummaryIndex, SummaryStatus};

/// All-blocks mask for doc-granular entries
pub const FULL_MASK: u8 = u8::MAX;

/// One posting: a document ordinal and the line blocks the gram touches
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Posting {
    pub ord: usize,
    pub mask: u8,
}

impl Posting {
    pub const fn full(ord: usize) -> Self {
        Self {
            ord,
            mask: FULL_MASK,
        }
    }
}

/// Whether gram co-occurrence is required per line block or per document
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Precision {
    Block,
    Doc,
}

pub trait PlanBackend {
    fn summaries(&self) -> &SummaryIndex;
    fn lookup_gram(&self, key: GramKey) -> anyhow::Result<Vec<Posting>>;
    fn forced_candidates(&self) -> anyhow::Result<Vec<usize>>;
}

pub fn execute<B: PlanBackend>(
    backend: &B,
    plan: &QueryPlan,
    precision: Precision,
) -> anyhow::Result<Vec<usize>> {
    let mut executor = Executor {
        backend,
        precision,
        cache: HashMap::new(),
    };
    let mut candidates = docs_of(executor.eval(plan.root())?);
    if !plan.is_none() {
        candidates = union_sorted(candidates, forced_candidates(backend, plan)?);
    }
    candidates.retain(|&ord| summary_may_satisfy(plan.root(), backend.summaries().status(ord)));
    Ok(candidates)
}

pub fn estimate_candidates<B: PlanBackend>(backend: &B, plan: &QueryPlan, df: &dyn DfStats) -> u64 {
    estimate_expr(backend.summaries(), plan.root(), df)
}

pub fn estimate_forced_candidates<B: PlanBackend>(
    backend: &B,
    plan: &QueryPlan,
) -> anyhow::Result<u64> {
    let count = forced_candidates(backend, plan)?.len();
    Ok(u64::try_from(count).unwrap_or(u64::MAX))
}

pub fn forced_candidates<B: PlanBackend>(
    backend: &B,
    plan: &QueryPlan,
) -> anyhow::Result<Vec<usize>> {
    if plan.is_none() {
        return Ok(Vec::new());
    }
    Ok(backend
        .forced_candidates()?
        .into_iter()
        .filter(|&ord| summary_may_satisfy(plan.root(), backend.summaries().status(ord)))
        .collect())
}

fn docs_of(postings: Vec<Posting>) -> Vec<usize> {
    let mut docs: Vec<usize> = postings.into_iter().map(|posting| posting.ord).collect();
    docs.dedup();
    docs
}

fn full_postings(ords: Vec<usize>) -> Vec<Posting> {
    ords.into_iter().map(Posting::full).collect()
}

fn estimate_expr(summaries: &SummaryIndex, expr: &PlanExpr, df: &dyn DfStats) -> u64 {
    let total = df.total_entries();
    match expr {
        PlanExpr::All => total,
        PlanExpr::None => 0,
        PlanExpr::AllOf {
            grams,
            needs,
            children,
        } => estimate_all_of(summaries, grams, needs, children, df, total),
        PlanExpr::AnyOf {
            grams,
            needs,
            children,
        } => estimate_any_of(summaries, grams, needs, children, df, total),
    }
}

fn estimate_all_of(
    summaries: &SummaryIndex,
    grams: &[GramNeedle],
    needs: &[ScanNeed],
    children: &[PlanExpr],
    df: &dyn DfStats,
    total: u64,
) -> u64 {
    let mut estimate = total;
    for needle in grams {
        estimate = estimate.min(estimate_needle(needle, df, total));
    }
    for child in children {
        estimate = estimate.min(estimate_expr(summaries, child, df));
    }
    if !grams.is_empty() || !children.is_empty() {
        return estimate;
    }
    needs
        .iter()
        .map(|need| count_need(summaries, need))
        .min()
        .unwrap_or(total)
}

fn estimate_any_of(
    summaries: &SummaryIndex,
    grams: &[GramNeedle],
    needs: &[ScanNeed],
    children: &[PlanExpr],
    df: &dyn DfStats,
    total: u64,
) -> u64 {
    let gram_estimate = grams
        .iter()
        .map(|needle| estimate_needle(needle, df, total))
        .fold(0u64, u64::saturating_add);
    let need_estimate = needs
        .iter()
        .map(|need| count_need(summaries, need))
        .fold(0u64, u64::saturating_add);
    let child_estimate = children
        .iter()
        .map(|child| estimate_expr(summaries, child, df))
        .fold(0u64, u64::saturating_add);
    gram_estimate
        .saturating_add(need_estimate)
        .saturating_add(child_estimate)
        .min(total)
}

fn estimate_needle(needle: &GramNeedle, df: &dyn DfStats, total: u64) -> u64 {
    needle
        .keys()
        .map(|key| df.entry_count(key))
        .fold(0u64, u64::saturating_add)
        .min(total)
}

fn count_need(summaries: &SummaryIndex, need: &ScanNeed) -> u64 {
    u64::try_from(summaries.count_satisfying(need)).unwrap_or(u64::MAX)
}

fn summary_may_satisfy(expr: &PlanExpr, status: SummaryStatus) -> bool {
    match expr {
        PlanExpr::All => status.is_text(),
        PlanExpr::None => false,
        PlanExpr::AllOf {
            grams,
            needs,
            children,
        } => {
            status.is_text()
                && grams.iter().all(needle_may_match)
                && needs.iter().all(|need| status.satisfies(need))
                && children
                    .iter()
                    .all(|child| summary_may_satisfy(child, status))
        },
        PlanExpr::AnyOf {
            grams,
            needs,
            children,
        } => {
            status.is_text()
                && (grams.iter().any(needle_may_match)
                    || needs.iter().any(|need| status.satisfies(need))
                    || children
                        .iter()
                        .any(|child| summary_may_satisfy(child, status)))
        },
    }
}

fn needle_may_match(needle: &GramNeedle) -> bool {
    match needle {
        GramNeedle::Key(_) => true,
        GramNeedle::AnyKey(keys) => !keys.is_empty(),
    }
}

struct Executor<'a, B> {
    backend: &'a B,
    precision: Precision,
    cache: HashMap<GramKey, Rc<Vec<Posting>>>,
}

impl<B: PlanBackend> Executor<'_, B> {
    fn eval(&mut self, expr: &PlanExpr) -> anyhow::Result<Vec<Posting>> {
        match expr {
            PlanExpr::All => Ok(full_postings(self.backend.summaries().text_ordinals())),
            PlanExpr::None => Ok(Vec::new()),
            PlanExpr::AllOf {
                grams,
                needs,
                children,
            } => self.eval_all_of(grams, needs, children),
            PlanExpr::AnyOf {
                grams,
                needs,
                children,
            } => self.eval_any_of(grams, needs, children),
        }
    }

    fn eval_all_of(
        &mut self,
        grams: &[GramNeedle],
        needs: &[ScanNeed],
        children: &[PlanExpr],
    ) -> anyhow::Result<Vec<Posting>> {
        let mut lists = Vec::with_capacity(grams.len() + children.len());
        for gram in grams {
            lists.push(Rc::new(self.eval_needle(gram)?));
        }
        for child in children {
            lists.push(Rc::new(self.eval(child)?));
        }
        let all_text = || full_postings(self.backend.summaries().text_ordinals());
        let mut candidates = intersect_all(lists, all_text);
        if !needs.is_empty() {
            candidates.retain(|posting| {
                let status = self.backend.summaries().status(posting.ord);
                needs.iter().all(|need| status.satisfies(need))
            });
        }
        Ok(candidates)
    }

    fn eval_any_of(
        &mut self,
        grams: &[GramNeedle],
        needs: &[ScanNeed],
        children: &[PlanExpr],
    ) -> anyhow::Result<Vec<Posting>> {
        let mut acc = Vec::new();
        for gram in grams {
            acc = union_postings(&acc, &self.eval_needle(gram)?);
        }
        for need in needs {
            let ords = self.backend.summaries().ordinals_satisfying(need);
            acc = union_postings(&acc, &full_postings(ords));
        }
        for child in children {
            acc = union_postings(&acc, &self.eval(child)?);
        }
        Ok(acc)
    }

    fn eval_needle(&mut self, needle: &GramNeedle) -> anyhow::Result<Vec<Posting>> {
        let mut acc = Vec::new();
        for key in needle.keys() {
            acc = union_postings(&acc, &self.lookup_cached(key)?);
        }
        Ok(acc)
    }

    fn lookup_cached(&mut self, key: GramKey) -> anyhow::Result<Rc<Vec<Posting>>> {
        if let Some(list) = self.cache.get(&key) {
            return Ok(Rc::clone(list));
        }
        let mut list = self.backend.lookup_gram(key)?;
        if self.precision == Precision::Doc {
            for posting in &mut list {
                posting.mask = FULL_MASK;
            }
        }
        let list = Rc::new(list);
        self.cache.insert(key, Rc::clone(&list));
        Ok(list)
    }
}

fn intersect_all(
    mut lists: Vec<Rc<Vec<Posting>>>,
    all_text: impl FnOnce() -> Vec<Posting>,
) -> Vec<Posting> {
    lists.sort_by_key(|list| list.len());
    let mut iter = lists.into_iter();
    let Some(first) = iter.next() else {
        return all_text();
    };
    let mut acc = first.as_ref().clone();
    for list in iter {
        acc = intersect_postings(&acc, &list);
        if acc.is_empty() {
            break;
        }
    }
    acc
}

/// Keep ordinals present in both lists whose block masks overlap
fn intersect_postings(left: &[Posting], right: &[Posting]) -> Vec<Posting> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < left.len() && j < right.len() {
        match left[i].ord.cmp(&right[j].ord) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                let mask = left[i].mask & right[j].mask;
                if mask != 0 {
                    out.push(Posting {
                        ord: left[i].ord,
                        mask,
                    });
                }
                i += 1;
                j += 1;
            },
        }
    }
    out
}

pub fn union_postings(left: &[Posting], right: &[Posting]) -> Vec<Posting> {
    let mut out = Vec::with_capacity(left.len() + right.len());
    let mut i = 0;
    let mut j = 0;
    while i < left.len() && j < right.len() {
        match left[i].ord.cmp(&right[j].ord) {
            std::cmp::Ordering::Less => {
                out.push(left[i]);
                i += 1;
            },
            std::cmp::Ordering::Greater => {
                out.push(right[j]);
                j += 1;
            },
            std::cmp::Ordering::Equal => {
                out.push(Posting {
                    ord: left[i].ord,
                    mask: left[i].mask | right[j].mask,
                });
                i += 1;
                j += 1;
            },
        }
    }
    out.extend_from_slice(&left[i..]);
    out.extend_from_slice(&right[j..]);
    out
}

pub fn union_sorted(left: Vec<usize>, right: Vec<usize>) -> Vec<usize> {
    let mut out = Vec::with_capacity(left.len() + right.len());
    let mut i = 0;
    let mut j = 0;
    while i < left.len() && j < right.len() {
        match left[i].cmp(&right[j]) {
            std::cmp::Ordering::Less => {
                out.push(left[i]);
                i += 1;
            },
            std::cmp::Ordering::Greater => {
                out.push(right[j]);
                j += 1;
            },
            std::cmp::Ordering::Equal => {
                out.push(left[i]);
                i += 1;
                j += 1;
            },
        }
    }
    out.extend_from_slice(&left[i..]);
    out.extend_from_slice(&right[j..]);
    out
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use sngram_types::{
        ByteSet256, EdgeBytes, GramKey, GramNeedle, PlanExpr, QueryPlan, SaturatingByteCounts256,
        ScanFlags, ScanNeed, ScanSummary,
    };

    use super::*;
    use crate::index::summary::{SummaryRecord, SummaryStatus};

    struct FakeBackend {
        summaries: SummaryIndex,
        grams: HashMap<GramKey, Vec<Posting>>,
        forced: Vec<usize>,
        lookups: RefCell<usize>,
    }

    impl PlanBackend for FakeBackend {
        fn summaries(&self) -> &SummaryIndex {
            &self.summaries
        }

        fn lookup_gram(&self, key: GramKey) -> anyhow::Result<Vec<Posting>> {
            *self.lookups.borrow_mut() += 1;
            Ok(self.grams.get(&key).cloned().unwrap_or_default())
        }

        fn forced_candidates(&self) -> anyhow::Result<Vec<usize>> {
            Ok(self.forced.clone())
        }
    }

    fn run(backend: &FakeBackend, plan: &QueryPlan) -> Vec<usize> {
        execute(backend, plan, Precision::Block).unwrap()
    }

    fn full(ords: &[usize]) -> Vec<Posting> {
        ords.iter().map(|&ord| Posting::full(ord)).collect()
    }

    fn masked(pairs: &[(usize, u8)]) -> Vec<Posting> {
        pairs
            .iter()
            .map(|&(ord, mask)| Posting { ord, mask })
            .collect()
    }

    #[test]
    fn all_of_intersects_grams_and_scan_needs() {
        let backend = fake_backend(&[(GramKey(1), full(&[0, 1]))], Vec::new());
        let plan = QueryPlan::new(PlanExpr::AllOf {
            grams: vec![GramNeedle::Key(GramKey(1))],
            needs: vec![ScanNeed::MinLineCount(2)],
            children: vec![],
        });

        assert_eq!(run(&backend, &plan), vec![1]);
    }

    #[test]
    fn all_of_rejects_disjoint_block_masks() {
        let backend = fake_backend(
            &[
                (GramKey(1), masked(&[(1, 0b0000_0001)])),
                (GramKey(2), masked(&[(1, 0b1000_0000)])),
            ],
            Vec::new(),
        );
        let plan = QueryPlan::new(PlanExpr::AllOf {
            grams: vec![GramNeedle::Key(GramKey(1)), GramNeedle::Key(GramKey(2))],
            needs: vec![],
            children: vec![],
        });

        assert_eq!(run(&backend, &plan), Vec::<usize>::new());
    }

    #[test]
    fn all_of_keeps_overlapping_block_masks() {
        let backend = fake_backend(
            &[
                (GramKey(1), masked(&[(1, 0b0000_0011)])),
                (GramKey(2), masked(&[(1, 0b0000_0010)])),
            ],
            Vec::new(),
        );
        let plan = QueryPlan::new(PlanExpr::AllOf {
            grams: vec![GramNeedle::Key(GramKey(1)), GramNeedle::Key(GramKey(2))],
            needs: vec![],
            children: vec![],
        });

        assert_eq!(run(&backend, &plan), vec![1]);
    }

    #[test]
    fn doc_precision_ignores_block_masks() {
        let backend = fake_backend(
            &[
                (GramKey(1), masked(&[(1, 0b0000_0001)])),
                (GramKey(2), masked(&[(1, 0b1000_0000)])),
            ],
            Vec::new(),
        );
        let plan = QueryPlan::new(PlanExpr::AllOf {
            grams: vec![GramNeedle::Key(GramKey(1)), GramNeedle::Key(GramKey(2))],
            needs: vec![],
            children: vec![],
        });

        assert_eq!(execute(&backend, &plan, Precision::Doc).unwrap(), vec![1]);
    }

    #[test]
    fn any_of_child_masks_flow_into_parent_intersection() {
        let backend = fake_backend(
            &[
                (GramKey(1), masked(&[(1, 0b0000_0001)])),
                (GramKey(2), masked(&[(1, 0b0000_0010)])),
                (GramKey(3), masked(&[(1, 0b0000_0001)])),
            ],
            Vec::new(),
        );
        let plan = QueryPlan::new(PlanExpr::AllOf {
            grams: vec![GramNeedle::Key(GramKey(3))],
            needs: vec![],
            children: vec![PlanExpr::AnyOf {
                grams: vec![GramNeedle::Key(GramKey(1)), GramNeedle::Key(GramKey(2))],
                needs: vec![],
                children: vec![],
            }],
        });

        assert_eq!(run(&backend, &plan), vec![1]);
    }

    #[test]
    fn any_of_unions_scan_needs_and_grams() {
        let backend = fake_backend(&[(GramKey(7), full(&[2]))], Vec::new());
        let plan = QueryPlan::new(PlanExpr::AnyOf {
            grams: vec![GramNeedle::Key(GramKey(7))],
            needs: vec![ScanNeed::MinLineCount(2)],
            children: vec![],
        });

        assert_eq!(run(&backend, &plan), vec![1, 2]);
    }

    #[test]
    fn forced_candidates_with_unknown_summary_are_retained_for_soundness() {
        let backend = fake_backend(&[], vec![2]);
        let plan = QueryPlan::new(PlanExpr::AllOf {
            grams: vec![GramNeedle::Key(GramKey(99))],
            needs: vec![ScanNeed::MinLineCount(9)],
            children: vec![],
        });

        assert_eq!(run(&backend, &plan), vec![2]);
    }

    #[test]
    fn forced_candidates_with_known_summary_must_satisfy_needs() {
        let backend = fake_backend(&[], vec![0, 1, 2]);
        let plan = QueryPlan::new(PlanExpr::AllOf {
            grams: vec![GramNeedle::Key(GramKey(99))],
            needs: vec![ScanNeed::MinLineCount(2)],
            children: vec![],
        });

        assert_eq!(run(&backend, &plan), vec![1, 2]);
    }

    #[test]
    fn forced_candidate_estimate_uses_execution_summary_filter() {
        let backend = fake_backend(&[], vec![0, 1, 2]);
        let plan = QueryPlan::new(PlanExpr::AllOf {
            grams: vec![GramNeedle::Key(GramKey(99))],
            needs: vec![ScanNeed::MinLineCount(2)],
            children: vec![],
        });
        let df = FakeDf { total: 3 };

        let sparse = estimate_candidates(&backend, &plan, &df);
        let forced = estimate_forced_candidates(&backend, &plan).unwrap();

        assert_eq!(sparse, 0);
        assert_eq!(forced, 2);
        assert_eq!(run(&backend, &plan).len() as u64, forced);
    }

    #[test]
    fn any_key_uses_one_lookup_per_key() {
        let backend = fake_backend(
            &[(GramKey(1), full(&[0, 2])), (GramKey(2), full(&[1, 2]))],
            Vec::new(),
        );
        let plan = QueryPlan::new(PlanExpr::AllOf {
            grams: vec![GramNeedle::AnyKey(vec![GramKey(1), GramKey(2)])],
            needs: vec![],
            children: vec![],
        });

        assert_eq!(run(&backend, &plan), vec![0, 1, 2]);
        assert_eq!(*backend.lookups.borrow(), 2);
    }

    #[test]
    fn impossible_composites_do_not_return_forced_candidates() {
        let backend = fake_backend(&[], vec![2]);
        let plans = [
            QueryPlan::new(PlanExpr::AnyOf {
                grams: vec![],
                needs: vec![],
                children: vec![],
            }),
            QueryPlan::new(PlanExpr::AnyOf {
                grams: vec![],
                needs: vec![],
                children: vec![PlanExpr::None],
            }),
            QueryPlan::new(PlanExpr::AllOf {
                grams: vec![],
                needs: vec![],
                children: vec![PlanExpr::None],
            }),
            QueryPlan::new(PlanExpr::AllOf {
                grams: vec![GramNeedle::AnyKey(Vec::new())],
                needs: vec![],
                children: vec![],
            }),
        ];

        for plan in plans {
            assert_eq!(run(&backend, &plan), Vec::<usize>::new());
        }
    }

    #[test]
    fn all_scan_need_variants_use_scan_need_satisfied_by() {
        let backend = fake_backend(&[], Vec::new());
        let needs = vec![
            ScanNeed::MinByteLen(4),
            ScanNeed::MinLineCount(2),
            ScanNeed::MinEmptyLineCount(1),
            ScanNeed::MinLongestLineLen(4),
            ScanNeed::HasFlags(ScanFlags::default().with_ascii_digit()),
            ScanNeed::ContainsAllBytes(byte_set(b"a1")),
            ScanNeed::ContainsAnyByte(byte_set(b"z1")),
            ScanNeed::MinByteCounts(Box::new(byte_counts(b"aa"))),
            ScanNeed::LineStartsWithAnyByte(byte_set(b"a")),
            ScanNeed::LineEndsWithAnyByte(byte_set(b"1")),
            ScanNeed::StartsWith(EdgeBytes::from_slice(b"aa")),
            ScanNeed::EndsWith(EdgeBytes::from_slice(b"11")),
        ];

        for need in needs {
            let plan = QueryPlan::new(PlanExpr::AllOf {
                grams: vec![],
                needs: vec![need],
                children: vec![],
            });
            assert_eq!(run(&backend, &plan), vec![1, 2]);
        }
    }

    fn fake_backend(pairs: &[(GramKey, Vec<Posting>)], forced: Vec<usize>) -> FakeBackend {
        let records = vec![
            SummaryRecord::new(0, SummaryStatus::Known(summary(1))),
            SummaryRecord::new(1, SummaryStatus::Known(rich_summary())),
            SummaryRecord::new(2, SummaryStatus::UnknownText),
        ];
        FakeBackend {
            summaries: SummaryIndex::from_records(records, 3).unwrap(),
            grams: pairs.iter().cloned().collect(),
            forced,
            lookups: RefCell::new(0),
        }
    }

    fn summary(lines: u32) -> ScanSummary {
        ScanSummary {
            byte_len: u64::from(lines),
            line_count: lines,
            empty_line_count: 0,
            longest_line_len: lines,
            gram_count: 0,
            flags: ScanFlags::default(),
            byte_counts: SaturatingByteCounts256::default(),
            line_start_bytes: ByteSet256::default(),
            line_end_bytes: ByteSet256::default(),
            prefix: EdgeBytes::default(),
            suffix: EdgeBytes::default(),
        }
    }

    fn rich_summary() -> ScanSummary {
        ScanSummary {
            byte_len: 6,
            line_count: 2,
            empty_line_count: 1,
            longest_line_len: 4,
            gram_count: 3,
            flags: ScanFlags::default()
                .with_ascii_lower()
                .with_ascii_digit()
                .with_lf(),
            byte_counts: byte_counts(b"aa11\n\n"),
            line_start_bytes: byte_set(b"a"),
            line_end_bytes: byte_set(b"1"),
            prefix: EdgeBytes::from_slice(b"aa11"),
            suffix: EdgeBytes::from_slice(b"11"),
        }
    }

    fn byte_set(bytes: &[u8]) -> ByteSet256 {
        let mut set = ByteSet256::default();
        for &byte in bytes {
            set.insert(byte);
        }
        set
    }

    fn byte_counts(bytes: &[u8]) -> SaturatingByteCounts256 {
        let mut counts = SaturatingByteCounts256::default();
        for &byte in bytes {
            counts.observe(byte);
        }
        counts
    }

    struct FakeDf {
        total: u64,
    }

    impl DfStats for FakeDf {
        fn entry_count(&self, _key: GramKey) -> u64 {
            0
        }

        fn total_entries(&self) -> u64 {
            self.total
        }
    }
}
