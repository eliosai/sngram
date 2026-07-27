//! Query planning from regex HIR to public plans.

use regex_syntax::hir::Hir;
use sngram_types::{PlanExpr, QueryError, QueryPlan, ScanNeed, WeightTable};

use super::{
    analyze::{Analyzer, PlanContext},
    needs::RootNeeds,
    parser,
    settings::QuerySettings,
    validate::ValidatedPattern,
};

mod lower;

/// Builds sparse-gram query plans against one weight table.
pub struct QueryPlanner<'a> {
    table: &'a WeightTable,
}

impl<'a> QueryPlanner<'a> {
    /// Bind the planner to a weight table.
    #[must_use]
    pub const fn new(table: &'a WeightTable) -> Self {
        Self { table }
    }

    /// Plan one validated regex pattern.
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::InvalidRegex`] when regex parsing fails.
    pub fn plan(&self, pattern: ValidatedPattern<'_>) -> Result<QueryPlan, QueryError> {
        let parsed = parser::parse(pattern)?;
        let fold = QuerySettings::CASE_FOLDED_SUPPLEMENTS && parsed.uses_folded_space();
        let ctx = PlanContext {
            fold,
            line_sentinels: QuerySettings::LINE_SENTINELS,
        };
        Ok(QueryPlan::new(self.plan_hir(parsed.hir(), ctx)))
    }

    fn plan_hir(&self, hir: &Hir, ctx: PlanContext) -> PlanExpr {
        let analyzer = Analyzer::with_context(self.table, ctx);
        with_root_needs(
            lower::query(analyzer.plan(hir), ctx.fold, hir),
            RootNeeds::from_hir(hir),
        )
    }
}

fn with_root_needs(expr: PlanExpr, needs: RootNeeds) -> PlanExpr {
    let needs = needs.into_vec();
    if needs.is_empty() || expr.is_none() {
        return expr;
    }
    append_root_needs(expr, needs)
}

fn append_root_needs(expr: PlanExpr, new_needs: Vec<ScanNeed>) -> PlanExpr {
    let (grams, needs, children) = match expr {
        PlanExpr::All => (vec![], new_needs, vec![]),
        PlanExpr::AllOf {
            grams,
            mut needs,
            children,
        } => {
            needs.extend(new_needs);
            (grams, needs, children)
        },
        other => (vec![], new_needs, vec![other]),
    };
    PlanExpr::AllOf {
        grams,
        needs,
        children,
    }
}

#[cfg(test)]
mod tests {
    use sngram_types::{GramNeedle, PlanExpr, WeightTable};

    use crate::query::query;

    fn table() -> WeightTable {
        WeightTable::from_weight_fn(|c1, c2| crc32fast::hash(&[c1, c2]))
    }

    /// The line-edge sides demanded across every needle of a plan's root
    fn line_edges(re: &str) -> Vec<(bool, bool)> {
        let plan = query(&table(), re).expect("pattern plans");
        let PlanExpr::AllOf { grams, .. } = plan.root() else {
            return Vec::new();
        };
        grams
            .iter()
            .filter_map(|needle| match needle {
                GramNeedle::AtLineEdge { starts, ends, .. } => Some((*starts, *ends)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn fully_anchored_literal_pins_both_of_its_edges() {
        let edges = line_edges("^kfree_skb;$");
        assert!(edges.iter().any(|&(starts, _)| starts), "{edges:?}");
        assert!(edges.iter().any(|&(_, ends)| ends), "{edges:?}");
    }

    #[test]
    fn a_gram_spanning_the_whole_literal_pins_both_sides_at_once() {
        let edges = line_edges("^done$");
        assert!(edges.contains(&(true, true)), "{edges:?}");
    }

    #[test]
    fn one_sided_anchors_pin_only_their_own_side() {
        assert!(line_edges("^kfree_skb").iter().all(|&(_, ends)| !ends));
        assert!(line_edges("kfree_skb$").iter().all(|&(starts, _)| !starts));
    }

    #[test]
    fn text_anchors_pin_line_edges_too() {
        assert!(!line_edges(r"\Akfree_skb\z").is_empty());
    }

    #[test]
    fn crlf_anchors_stay_unpinned() {
        assert!(
            line_edges("(?R:^kfree_skb$)").is_empty(),
            "a CRLF `$` also matches before a `\\r`, which the stored bits do not record"
        );
    }

    #[test]
    fn unanchored_and_nonliteral_shapes_stay_unpinned() {
        assert!(line_edges("kfree_skb").is_empty());
        assert!(line_edges(r"^[ \t]*kfree_skb").is_empty());
        assert!(line_edges("(?i:^kfree_skb$)").is_empty());
    }
}
