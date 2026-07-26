//! Direct execution of the common all-of plan: one driver list, the rest filters.

use sngram_types::{GramNeedle, PlanExpr, QueryPlan, ScanNeed};

use super::PostingsIndex;
use crate::index::executor::{self, combine};

pub struct FastAllOf<'a> {
    index: &'a PostingsIndex,
    driver: FastNeedle,
    filters: Vec<FastNeedle>,
    needs: &'a [ScanNeed],
    precision: executor::Precision,
}

impl<'a> FastAllOf<'a> {
    pub fn try_execute(
        index: &'a PostingsIndex,
        plan: &'a QueryPlan,
        precision: executor::Precision,
    ) -> anyhow::Result<Option<Vec<usize>>> {
        let Some(query) = Self::from_plan(index, plan, precision)? else {
            return Ok(None);
        };
        Ok(Some(query.execute()))
    }

    fn from_plan(
        index: &'a PostingsIndex,
        plan: &'a QueryPlan,
        precision: executor::Precision,
    ) -> anyhow::Result<Option<Self>> {
        let PlanExpr::AllOf {
            grams,
            needs,
            children,
        } = plan.root()
        else {
            return Ok(None);
        };
        if grams.is_empty() || !children.is_empty() {
            return Ok(None);
        }
        let mut needles = Self::needles(index, grams)?;
        needles.sort_by_key(FastNeedle::len);
        let driver = needles.remove(0);
        Ok(Some(Self {
            index,
            driver,
            filters: needles,
            needs,
            precision,
        }))
    }

    fn needles(
        index: &'a PostingsIndex,
        grams: &'a [GramNeedle],
    ) -> anyhow::Result<Vec<FastNeedle>> {
        grams
            .iter()
            .map(|needle| FastNeedle::open(index, needle))
            .collect()
    }

    fn execute(mut self) -> Vec<usize> {
        let driver = self.driver.postings();
        let mut candidates = Vec::new();
        for posting in driver {
            if self.keeps(posting) {
                candidates.push(posting.ord);
            }
        }
        candidates
    }

    fn keeps(&mut self, posting: executor::Posting) -> bool {
        if !self.index.summaries.is_text(posting.ord) {
            return false;
        }
        let precision = self.precision;
        let mut blocks = effective(precision, posting.mask) & executor::BLOCK_BITS;
        for filter in &mut self.filters {
            let Some(filter_mask) = filter.mask_at(posting.ord) else {
                return false;
            };
            blocks &= effective(precision, filter_mask);
            if blocks == 0 {
                return false;
            }
        }
        self.index.summaries.meets(posting.ord, self.needs)
    }
}

/// The block bits a mask contributes at this precision
const fn effective(precision: executor::Precision, mask: u16) -> u16 {
    match precision {
        executor::Precision::Block => mask,
        executor::Precision::Doc => mask | executor::BLOCK_BITS,
    }
}

struct FastNeedle {
    lists: Vec<Vec<executor::Posting>>,
    cursors: Vec<usize>,
    len: usize,
}

impl FastNeedle {
    fn open(index: &PostingsIndex, needle: &GramNeedle) -> anyhow::Result<Self> {
        let required = executor::required_edges(needle);
        let mut lists = needle
            .keys()
            .map(|key| index.lookup(key.value()))
            .collect::<anyhow::Result<Vec<_>>>()?;
        if required != 0 {
            for list in &mut lists {
                list.retain(|posting| posting.mask & required == required);
            }
        }
        let len = lists.iter().map(Vec::len).sum();
        Ok(Self {
            cursors: vec![0; lists.len()],
            lists,
            len,
        })
    }

    const fn len(&self) -> usize {
        self.len
    }

    fn postings(&self) -> Vec<executor::Posting> {
        combine::union_all(&combine::borrowed(&self.lists))
    }

    /// The mask this needle holds at one ordinal, probed in ascending order
    fn mask_at(&mut self, ord: usize) -> Option<u16> {
        let mut mask = 0u16;
        for (list, at) in self.lists.iter().zip(self.cursors.iter_mut()) {
            *at = gallop_to(list, *at, ord);
            if let Some(posting) = list.get(*at)
                && posting.ord == ord
            {
                mask |= posting.mask;
            }
        }
        (mask != 0).then_some(mask)
    }
}

/// First index at or after `from` whose ordinal reaches `ord`
fn gallop_to(list: &[executor::Posting], from: usize, ord: usize) -> usize {
    let len = list.len();
    let mut lo = from.min(len);
    if lo == len {
        return len;
    }
    let mut step = 1usize;
    while lo + step < len && list[lo + step].ord < ord {
        lo += step;
        step *= 2;
    }
    let hi = lo.saturating_add(step).saturating_add(1).min(len);
    lo + list[lo..hi].partition_point(|posting| posting.ord < ord)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(ords: &[usize]) -> Vec<executor::Posting> {
        ords.iter()
            .map(|&ord| executor::Posting {
                ord,
                mask: 1 << (ord % 10),
            })
            .collect()
    }

    /// The whole-list binary search the cursor walk replaced
    fn searched(lists: &[Vec<executor::Posting>], ord: usize) -> Option<u16> {
        let mut mask = 0u16;
        for list in lists {
            if let Ok(idx) = list.binary_search_by_key(&ord, |posting| posting.ord) {
                mask |= list[idx].mask;
            }
        }
        (mask != 0).then_some(mask)
    }

    fn needle(lists: Vec<Vec<executor::Posting>>) -> FastNeedle {
        let len = lists.iter().map(Vec::len).sum();
        FastNeedle {
            cursors: vec![0; lists.len()],
            lists,
            len,
        }
    }

    #[test]
    fn cursor_probes_match_a_fresh_binary_search() {
        let lists = vec![
            list(&[0, 3, 4, 9, 40, 41, 900]),
            list(&[1, 3, 8, 9, 10, 500]),
            list(&[]),
            list(&[900]),
        ];
        let mut probed = needle(lists.clone());

        for ord in 0..1000 {
            assert_eq!(probed.mask_at(ord), searched(&lists, ord), "ordinal {ord}");
        }
    }

    #[test]
    fn cursor_probes_match_when_most_ordinals_are_skipped() {
        let dense: Vec<usize> = (0..5000).map(|ord| ord * 3).collect();
        let lists = vec![list(&dense)];
        let mut probed = needle(lists.clone());

        for ord in [0usize, 3, 7, 4242, 8000, 14_997, 14_998, 40_000] {
            assert_eq!(probed.mask_at(ord), searched(&lists, ord), "ordinal {ord}");
        }
    }

    #[test]
    fn gallop_lands_on_the_first_ordinal_at_or_past_the_target() {
        let postings = list(&[2, 4, 6, 8, 10]);

        assert_eq!(gallop_to(&postings, 0, 0), 0);
        assert_eq!(gallop_to(&postings, 0, 5), 2);
        assert_eq!(gallop_to(&postings, 2, 6), 2);
        assert_eq!(gallop_to(&postings, 0, 11), 5);
        assert_eq!(gallop_to(&postings, 5, 1), 5);
        assert_eq!(gallop_to(&postings, 9, 1), 5);
    }
}
