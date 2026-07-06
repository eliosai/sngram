//! Complete execution of public sparse-query plans against an eg index.

use std::{collections::HashMap, rc::Rc};

use sngram_types::{DfStats, GramKey, GramNeedle, PlanExpr, QueryPlan, ScanNeed};

use super::summary::{SummaryIndex, SummaryStatus};

pub trait PlanBackend {
    fn summaries(&self) -> &SummaryIndex;
    fn lookup_gram(&self, key: GramKey) -> anyhow::Result<Vec<usize>>;
    fn forced_candidates(&self) -> anyhow::Result<Vec<usize>>;
}

pub fn execute<B: PlanBackend>(backend: &B, plan: &QueryPlan) -> anyhow::Result<Vec<usize>> {
    let mut executor = Executor {
        backend,
        cache: HashMap::new(),
    };
    let mut candidates = executor.eval(plan.root())?;
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
    let gram_estimates = grams
        .iter()
        .map(|needle| estimate_needle(needle, df, total));
    let need_estimates = needs.iter().map(|need| count_need(summaries, need));
    let child_estimates = children
        .iter()
        .map(|child| estimate_expr(summaries, child, df));
    gram_estimates
        .chain(need_estimates)
        .chain(child_estimates)
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
    cache: HashMap<GramKey, Rc<Vec<usize>>>,
}

impl<B: PlanBackend> Executor<'_, B> {
    fn eval(&mut self, expr: &PlanExpr) -> anyhow::Result<Vec<usize>> {
        match expr {
            PlanExpr::All => Ok(self.backend.summaries().text_ordinals()),
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
    ) -> anyhow::Result<Vec<usize>> {
        let mut lists = Vec::with_capacity(grams.len() + children.len());
        for gram in grams {
            lists.push(Rc::new(self.eval_needle(gram)?));
        }
        for child in children {
            lists.push(Rc::new(self.eval(child)?));
        }
        let mut candidates = intersect_all(lists, self.backend.summaries().text_ordinals());
        if !needs.is_empty() {
            candidates.retain(|&ord| {
                let status = self.backend.summaries().status(ord);
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
    ) -> anyhow::Result<Vec<usize>> {
        let mut acc = Vec::new();
        for gram in grams {
            acc.extend(self.eval_needle(gram)?);
        }
        for need in needs {
            acc.extend(self.backend.summaries().ordinals_satisfying(need));
        }
        for child in children {
            acc.extend(self.eval(child)?);
        }
        acc.sort_unstable();
        acc.dedup();
        Ok(acc)
    }

    fn eval_needle(&mut self, needle: &GramNeedle) -> anyhow::Result<Vec<usize>> {
        let mut acc = Vec::new();
        for key in needle.keys() {
            acc.extend_from_slice(&self.lookup_cached(key)?);
        }
        acc.sort_unstable();
        acc.dedup();
        Ok(acc)
    }

    fn lookup_cached(&mut self, key: GramKey) -> anyhow::Result<Rc<Vec<usize>>> {
        if let Some(list) = self.cache.get(&key) {
            return Ok(Rc::clone(list));
        }
        let list = Rc::new(self.backend.lookup_gram(key)?);
        self.cache.insert(key, Rc::clone(&list));
        Ok(list)
    }
}

fn intersect_all(mut lists: Vec<Rc<Vec<usize>>>, all_text: Vec<usize>) -> Vec<usize> {
    lists.sort_by_key(|list| list.len());
    let mut iter = lists.into_iter();
    let Some(first) = iter.next() else {
        return all_text;
    };
    let mut acc = first.as_ref().clone();
    for list in iter {
        acc = intersect_sorted(&acc, &list);
        if acc.is_empty() {
            break;
        }
    }
    acc
}

fn intersect_sorted(left: &[usize], right: &[usize]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < left.len() && j < right.len() {
        match left[i].cmp(&right[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(left[i]);
                i += 1;
                j += 1;
            },
        }
    }
    out
}

pub fn union_sorted(left: Vec<usize>, right: Vec<usize>) -> Vec<usize> {
    union_sorted_ref(&left, &right)
}

fn union_sorted_ref(left: &[usize], right: &[usize]) -> Vec<usize> {
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
        grams: HashMap<GramKey, Vec<usize>>,
        forced: Vec<usize>,
        lookups: RefCell<usize>,
    }

    impl PlanBackend for FakeBackend {
        fn summaries(&self) -> &SummaryIndex {
            &self.summaries
        }

        fn lookup_gram(&self, key: GramKey) -> anyhow::Result<Vec<usize>> {
            *self.lookups.borrow_mut() += 1;
            Ok(self.grams.get(&key).cloned().unwrap_or_default())
        }

        fn forced_candidates(&self) -> anyhow::Result<Vec<usize>> {
            Ok(self.forced.clone())
        }
    }

    #[test]
    fn all_of_intersects_grams_and_scan_needs() {
        let backend = fake_backend(&[(GramKey(1), vec![0, 1])], Vec::new());
        let plan = QueryPlan::new(PlanExpr::AllOf {
            grams: vec![GramNeedle::Key(GramKey(1))],
            needs: vec![ScanNeed::MinLineCount(2)],
            children: vec![],
        });

        assert_eq!(execute(&backend, &plan).unwrap(), vec![1]);
    }

    #[test]
    fn any_of_unions_scan_needs_and_grams() {
        let backend = fake_backend(&[(GramKey(7), vec![2])], Vec::new());
        let plan = QueryPlan::new(PlanExpr::AnyOf {
            grams: vec![GramNeedle::Key(GramKey(7))],
            needs: vec![ScanNeed::MinLineCount(2)],
            children: vec![],
        });

        assert_eq!(execute(&backend, &plan).unwrap(), vec![1, 2]);
    }

    #[test]
    fn forced_candidates_with_unknown_summary_are_retained_for_soundness() {
        let backend = fake_backend(&[], vec![2]);
        let plan = QueryPlan::new(PlanExpr::AllOf {
            grams: vec![GramNeedle::Key(GramKey(99))],
            needs: vec![ScanNeed::MinLineCount(9)],
            children: vec![],
        });

        assert_eq!(execute(&backend, &plan).unwrap(), vec![2]);
    }

    #[test]
    fn forced_candidates_with_known_summary_must_satisfy_needs() {
        let backend = fake_backend(&[], vec![0, 1, 2]);
        let plan = QueryPlan::new(PlanExpr::AllOf {
            grams: vec![GramNeedle::Key(GramKey(99))],
            needs: vec![ScanNeed::MinLineCount(2)],
            children: vec![],
        });

        assert_eq!(execute(&backend, &plan).unwrap(), vec![1, 2]);
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
        assert_eq!(execute(&backend, &plan).unwrap().len() as u64, forced);
    }

    #[test]
    fn any_key_uses_one_lookup_per_key() {
        let backend = fake_backend(
            &[(GramKey(1), vec![0, 2]), (GramKey(2), vec![1, 2])],
            Vec::new(),
        );
        let plan = QueryPlan::new(PlanExpr::AllOf {
            grams: vec![GramNeedle::AnyKey(vec![GramKey(1), GramKey(2)])],
            needs: vec![],
            children: vec![],
        });

        assert_eq!(execute(&backend, &plan).unwrap(), vec![0, 1, 2]);
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
            assert_eq!(execute(&backend, &plan).unwrap(), Vec::<usize>::new());
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
            assert_eq!(execute(&backend, &plan).unwrap(), vec![1, 2]);
        }
    }

    fn fake_backend(pairs: &[(GramKey, Vec<usize>)], forced: Vec<usize>) -> FakeBackend {
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
