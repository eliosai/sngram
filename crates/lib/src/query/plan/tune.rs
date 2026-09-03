//! Df-driven query plan tuning.

use super::{DfStats, GramNeedle, PlanExpr, QueryPlan};

impl QueryPlan {
    /// Reorder and thin the plan by df
    pub fn tune(&mut self, df: &dyn DfStats, stop_df: u64) {
        self.root.tune(df, stop_df);
    }
}

impl PlanExpr {
    const MAX_ALL_OF_GRAMS: usize = 32;

    fn tune(&mut self, df: &dyn DfStats, stop_df: u64) {
        match self {
            Self::All | Self::None => {},
            Self::AllOf {
                grams, children, ..
            } => {
                let keep_first = children.is_empty();
                Self::sort_grams_by_df(grams, df);
                Self::retain_selective_grams(grams, keep_first, df, stop_df);
                Self::tune_children(children, df, stop_df);
                Self::drop_weak_bags(grams, children, df, stop_df);
            },
            Self::AnyOf {
                grams, children, ..
            } => {
                Self::sort_grams_by_df(grams, df);
                Self::tune_children(children, df, stop_df);
            },
        }
    }

    /// Drop weak gram bags when a stronger sibling remains
    fn drop_weak_bags(
        grams: &[GramNeedle],
        children: &mut Vec<Self>,
        df: &dyn DfStats,
        stop_df: u64,
    ) {
        let weak: Vec<bool> = children
            .iter()
            .map(|child| Self::is_weak_bag(child, df, stop_df))
            .collect();
        let strong_left = grams.len() + weak.iter().filter(|&&flag| !flag).count();
        if strong_left == 0 {
            return;
        }
        let mut flags = weak.into_iter();
        children.retain(|_| !flags.next().unwrap_or(false));
    }

    fn is_weak_bag(expr: &Self, df: &dyn DfStats, stop_df: u64) -> bool {
        let Self::AnyOf {
            grams,
            needs,
            children,
        } = expr
        else {
            return false;
        };
        if !needs.is_empty() || !children.is_empty() {
            return false;
        }
        grams
            .iter()
            .map(|gram| gram.estimate_candidates(df))
            .fold(0u64, u64::saturating_add)
            >= stop_df
    }

    fn retain_selective_grams(
        grams: &mut Vec<GramNeedle>,
        keep_first: bool,
        df: &dyn DfStats,
        stop_df: u64,
    ) {
        let mut kept = 0usize;
        grams.retain(|gram| {
            kept += 1;
            ((keep_first && kept == 1) || gram.estimate_candidates(df) < stop_df)
                && kept <= Self::MAX_ALL_OF_GRAMS
        });
    }

    fn sort_grams_by_df(grams: &mut [GramNeedle], df: &dyn DfStats) {
        for needle in grams.iter_mut() {
            needle.sort_keys_by_df(df);
        }
        grams.sort_by_cached_key(|gram| gram.estimate_candidates(df));
    }

    fn tune_children(children: &mut [Self], df: &dyn DfStats, stop_df: u64) {
        for child in children {
            child.tune(df, stop_df);
        }
    }
}

impl GramNeedle {
    fn estimate_candidates(&self, df: &dyn DfStats) -> u64 {
        let total = df.total_entries();
        match self {
            Self::Key(key) => df.entry_count(*key).min(total),
            Self::AnyKey(keys) | Self::AtWordEdge { keys, .. } | Self::AtLineEdge { keys, .. } => {
                keys.iter()
                    .map(|&key| df.entry_count(key))
                    .sum::<u64>()
                    .min(total)
            },
        }
    }

    fn sort_keys_by_df(&mut self, df: &dyn DfStats) {
        if let Self::AnyKey(keys) | Self::AtWordEdge { keys, .. } | Self::AtLineEdge { keys, .. } =
            self
        {
            keys.sort_by_cached_key(|&key| df.entry_count(key));
            keys.dedup();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::GramKey;

    struct MapDf {
        counts: HashMap<GramKey, u64>,
        total: u64,
    }

    impl DfStats for MapDf {
        fn entry_count(&self, key: GramKey) -> u64 {
            self.counts.get(&key).copied().unwrap_or(0)
        }

        fn total_entries(&self) -> u64 {
            self.total
        }
    }

    fn df_of(pairs: &[(GramKey, u64)], total: u64) -> MapDf {
        MapDf {
            counts: pairs.iter().copied().collect(),
            total,
        }
    }

    fn key(value: u64) -> GramKey {
        GramKey::new(value)
    }

    #[test]
    fn gram_estimates_bound_and_by_rarest_or_by_sum() {
        let df = df_of(&[(key(1), 900), (key(2), 3)], 1000);
        let and = PlanExpr::AllOf {
            grams: vec![GramNeedle::Key(key(1)), GramNeedle::Key(key(2))],
            needs: vec![],
            children: vec![],
        };
        let or = PlanExpr::AnyOf {
            grams: vec![GramNeedle::Key(key(1)), GramNeedle::Key(key(2))],
            needs: vec![],
            children: vec![],
        };

        assert_eq!(estimate_expr_candidates(&and, &df), 3);
        assert_eq!(estimate_expr_candidates(&or, &df), 903);
        assert_eq!(estimate_expr_candidates(&PlanExpr::All, &df), 1000);
        assert_eq!(estimate_expr_candidates(&PlanExpr::None, &df), 0);
    }

    fn all_of_keys(values: &[u64]) -> QueryPlan {
        QueryPlan::new(PlanExpr::AllOf {
            grams: values.iter().map(|&v| GramNeedle::Key(key(v))).collect(),
            needs: vec![],
            children: vec![],
        })
    }

    #[test]
    fn tuning_caps_all_of_grams_to_rarest_few() {
        let df = df_of(
            &[
                (key(1), 10),
                (key(2), 20),
                (key(3), 30),
                (key(4), 40),
                (key(5), 50),
            ],
            1000,
        );
        let mut plan = all_of_keys(&[5, 4, 3, 2, 1]);

        plan.tune(&df, 45);

        let PlanExpr::AllOf { grams, .. } = plan.root() else {
            panic!("tuned plan must stay AllOf");
        };
        assert_eq!(
            grams,
            &[
                GramNeedle::Key(key(1)),
                GramNeedle::Key(key(2)),
                GramNeedle::Key(key(3)),
                GramNeedle::Key(key(4)),
            ]
        );
    }

    fn weak_bag() -> PlanExpr {
        PlanExpr::AnyOf {
            grams: vec![GramNeedle::Key(key(2)), GramNeedle::Key(key(3))],
            needs: vec![],
            children: vec![],
        }
    }

    fn all_of_children(grams: Vec<GramNeedle>, children: Vec<PlanExpr>) -> QueryPlan {
        QueryPlan::new(PlanExpr::AllOf {
            grams,
            needs: vec![],
            children,
        })
    }

    #[test]
    fn tuning_drops_unselective_bags_beside_stronger_constraints() {
        let df = df_of(&[(key(1), 5), (key(2), 600), (key(3), 700)], 1000);
        let mut plan = all_of_children(vec![GramNeedle::Key(key(1))], vec![weak_bag()]);

        plan.tune(&df, 300);

        let PlanExpr::AllOf { children, .. } = plan.root() else {
            panic!("tuned plan must stay AllOf");
        };
        assert!(children.is_empty(), "weak bag should drop: {plan}");
    }

    #[test]
    fn tuning_keeps_a_weak_bag_that_is_the_last_constraint() {
        let df = df_of(&[(key(2), 600), (key(3), 700)], 1000);
        let mut lone = all_of_children(vec![], vec![weak_bag()]);

        lone.tune(&df, 300);

        let PlanExpr::AllOf { children, .. } = lone.root() else {
            panic!("tuned plan must stay AllOf");
        };
        assert_eq!(children.len(), 1, "last constraint must survive: {lone}");
    }

    fn estimate_expr_candidates(expr: &PlanExpr, df: &dyn DfStats) -> u64 {
        let total = df.total_entries();
        match expr {
            PlanExpr::All => total,
            PlanExpr::None => 0,
            PlanExpr::AllOf {
                grams, children, ..
            } => estimate_all_candidates(grams, children, df),
            PlanExpr::AnyOf {
                grams, children, ..
            } => estimate_any_candidates(grams, children, df),
        }
    }

    fn estimate_all_candidates(
        grams: &[GramNeedle],
        children: &[PlanExpr],
        df: &dyn DfStats,
    ) -> u64 {
        let grams = grams.iter().map(|gram| gram.estimate_candidates(df)).min();
        let children = children
            .iter()
            .map(|child| estimate_expr_candidates(child, df))
            .min();
        grams
            .into_iter()
            .chain(children)
            .min()
            .unwrap_or_else(|| df.total_entries())
    }

    fn estimate_any_candidates(
        grams: &[GramNeedle],
        children: &[PlanExpr],
        df: &dyn DfStats,
    ) -> u64 {
        let grams = grams
            .iter()
            .map(|gram| gram.estimate_candidates(df))
            .fold(0u64, u64::saturating_add);
        let children = children
            .iter()
            .map(|child| estimate_expr_candidates(child, df))
            .fold(0u64, u64::saturating_add);
        grams.saturating_add(children).min(df.total_entries())
    }
}
