//! Query planning from regex HIR to public plans.

use regex_syntax::hir::Hir;
use sngram_types::{
    Gram, GramKey, GramNeedle, HashKey, PlanExpr, QueryError, QueryPlan, WeightTable,
};

use super::{
    algebra::{Op, Query},
    analyze::{Analyzer, PlanContext},
    parser::QueryParser,
    settings::QuerySettings,
    strings::StringSet,
    validate::ValidatedPattern,
};

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
        let parsed = QueryParser::parse(pattern)?;
        let fold = QuerySettings::CASE_FOLDED_SUPPLEMENTS && parsed.uses_folded_space();
        let ctx = PlanContext {
            fold,
            line_sentinels: QuerySettings::LINE_SENTINELS,
        };
        Ok(QueryPlan::new(self.plan_hir(parsed.hir(), ctx)))
    }

    fn plan_hir(&self, hir: &Hir, ctx: PlanContext) -> PlanExpr {
        let analyzer = Analyzer::with_context(self.table, ctx);
        into_public_expr(analyzer.plan(hir), ctx.fold)
    }
}

fn into_public_expr(query: Query, fold: bool) -> PlanExpr {
    match query.op {
        Op::All => PlanExpr::All,
        Op::None => PlanExpr::None,
        Op::And => PlanExpr::AllOf {
            grams: public_grams(query.grams, fold),
            needs: Vec::new(),
            children: public_children(query.sub, fold),
        },
        Op::Or => PlanExpr::AnyOf {
            grams: public_grams(query.grams, fold),
            needs: Vec::new(),
            children: public_children(query.sub, fold),
        },
    }
}

fn public_grams(grams: StringSet, fold: bool) -> Vec<GramNeedle> {
    grams
        .into_vec()
        .into_iter()
        .map(|gram| needle_for(&gram, fold))
        .collect()
}

fn public_children(children: Vec<Query>, fold: bool) -> Vec<PlanExpr> {
    children
        .into_iter()
        .map(|query| into_public_expr(query, fold))
        .collect()
}

fn needle_for(gram: &Gram, fold: bool) -> GramNeedle {
    let raw = GramKey(HashKey::UNKEYED.hash_bytes(gram.as_bytes()));
    if !fold || !gram.as_bytes().iter().any(u8::is_ascii_alphabetic) {
        return GramNeedle::Key(raw);
    }
    GramNeedle::AnyKey(vec![
        raw,
        GramKey(HashKey::UNKEYED.folded().hash_bytes(gram.as_bytes())),
    ])
}

#[cfg(test)]
mod tests {
    //! Structure tests for regex query planning. End-to-end soundness lives in
    //! `tests/soundness.rs`.

    use std::collections::HashMap;

    use sngram_types::{
        DfStats, GramKey, GramNeedle, HashKey, PlanExpr, QueryError, QueryPlan, WeightTable,
    };

    use crate::query::query;

    fn table() -> WeightTable {
        WeightTable::from_weight_fn(|c1, c2| crc32fast::hash(&[c1, c2]))
    }

    fn plan_of(re: &str) -> QueryPlan {
        query(&table(), re).expect("pattern parses")
    }

    fn expr_of(re: &str) -> PlanExpr {
        plan_of(re).root().clone()
    }

    fn shape(expr: &PlanExpr) -> String {
        match expr {
            PlanExpr::All => "+".to_string(),
            PlanExpr::None => "-".to_string(),
            PlanExpr::AllOf {
                grams, children, ..
            } => shape_join(grams, children, "G", " & "),
            PlanExpr::AnyOf {
                grams, children, ..
            } => shape_join(grams, children, "O", " | "),
        }
    }

    fn shape_join(grams: &[GramNeedle], children: &[PlanExpr], bag: &str, sep: &str) -> String {
        let mut parts = Vec::new();
        if !grams.is_empty() {
            parts.push(bag.to_string());
        }
        parts.extend(children.iter().map(shape));
        if parts.len() == 1 {
            return parts.pop().expect("len 1");
        }
        format!("({})", parts.join(sep))
    }

    fn has_or(expr: &PlanExpr) -> bool {
        match expr {
            PlanExpr::AnyOf { .. } => true,
            PlanExpr::AllOf { children, .. } => children.iter().any(has_or),
            _ => false,
        }
    }

    fn assert_shape(re: &str, expected: &str) {
        assert_eq!(shape(&expr_of(re)), expected, "shape mismatch for {re:?}");
    }

    #[test]
    fn literal_pattern_extracts_grams() {
        assert!(matches!(expr_of("MAX_FILE_SIZE"), PlanExpr::AllOf { .. }));
    }

    #[test]
    fn broad_patterns_are_all() {
        assert!(plan_of(".*").is_all());
        assert!(plan_of(r"[a-z]+").is_all());
        assert!(plan_of("ab").is_all());
    }

    #[test]
    fn invalid_patterns_return_errors() {
        let long = "a".repeat(5000);
        let err = query(&table(), &long).unwrap_err();
        assert!(matches!(err, QueryError::PatternTooLong { .. }));

        let err = query(&table(), "(unclosed").unwrap_err();
        assert!(matches!(err, QueryError::InvalidRegex(_)));
    }

    #[test]
    fn anchors_and_boundaries_prune_impossible_patterns() {
        assert_ne!(expr_of("^abc"), expr_of("abc"));
        assert_ne!(expr_of("abc$"), expr_of("abc"));
        assert_eq!(expr_of(r"\babc"), expr_of("abc"));
        assert_eq!(expr_of(r"ab\b-cd"), expr_of("ab-cd"));
        assert!(matches!(expr_of(r"ab\bc"), PlanExpr::None));
        assert!(matches!(expr_of(r"foo$bar"), PlanExpr::None));
    }

    #[test]
    fn alternation_and_repetition_keep_selective_constraints() {
        assert!(matches!(
            expr_of("(a+hello|b+world)"),
            PlanExpr::AnyOf { .. }
        ));
        assert!(!expr_of("a{3,5}bcdef").is_all());
        assert!(!expr_of("foo[α-γ]bar").is_all());
        assert_eq!(expr_of("ab[cd]ef"), expr_of("abcef|abdef"));
        assert_eq!(expr_of("x{5}"), expr_of("xxxxx"));
        assert_eq!(expr_of("h{3,5}i"), expr_of("hhhi|hhhhi|hhhhhi"));
    }

    #[test]
    fn inline_case_insensitive_uses_key_alternatives() {
        let plan = plan_of("(?i)netif_receive_skb_list_internal");
        assert!(plan.gram_count() > 0);
        assert!(has_any_key(plan.root()));
    }

    #[test]
    fn sensitive_queries_use_single_keys() {
        let plan = plan_of("SchedClock");
        assert!(!has_any_key(plan.root()));
    }

    fn has_any_key(expr: &PlanExpr) -> bool {
        match expr {
            PlanExpr::All | PlanExpr::None => false,
            PlanExpr::AllOf {
                grams, children, ..
            }
            | PlanExpr::AnyOf {
                grams, children, ..
            } => {
                grams
                    .iter()
                    .any(|needle| matches!(needle, GramNeedle::AnyKey(_)))
                    || children.iter().any(has_any_key)
            },
        }
    }

    #[test]
    fn exact_base_repetition_above_cap_expands_to_literal() {
        assert_eq!(expr_of("ab{5}cd"), expr_of("abbbbbcd"));
        assert_eq!(expr_of("(abc){5}"), expr_of("abcabcabcabcabc"));
        assert_eq!(expr_of("a{8}"), expr_of("aaaaaaaa"));
    }

    #[test]
    fn nested_alternations_both_survive() {
        let expr = expr_of("(z*abcz*defz*)(z*(ghi|jkl)z*)");
        assert!(has_or(&expr), "alternation lost in {}", shape(&expr));
        assert!(
            shape(&expr).contains('&'),
            "concat lost in {}",
            shape(&expr)
        );
    }

    #[test]
    fn display_matches_codesearch_string_forms() {
        assert_eq!(plan_of(".").to_string(), "+");
        assert_eq!(plan_of(r"[^\s\S]").to_string(), "-");
        assert!(plan_of("(a+hello|b+world)").to_string().contains('|'));
    }

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

    fn df_of(pairs: &[(&[u8], u64)], total: u64) -> MapDf {
        MapDf {
            counts: pairs.iter().map(|(gram, n)| (key(gram), *n)).collect(),
            total,
        }
    }

    fn key(bytes: &[u8]) -> GramKey {
        GramKey(HashKey::UNKEYED.hash_bytes(bytes))
    }

    fn plan(expr: PlanExpr) -> QueryPlan {
        QueryPlan::new(expr)
    }

    #[test]
    fn estimate_bounds_and_by_rarest_and_or_by_sum() {
        let and = plan(PlanExpr::AllOf {
            grams: vec![GramNeedle::Key(key(b"abc")), GramNeedle::Key(key(b"xyz"))],
            needs: vec![],
            children: vec![],
        });
        let df = df_of(&[(b"abc", 900), (b"xyz", 3)], 1000);
        assert_eq!(and.estimate_candidates(&df), 3);

        let or = plan(PlanExpr::AnyOf {
            grams: vec![GramNeedle::Key(key(b"abc")), GramNeedle::Key(key(b"xyz"))],
            needs: vec![],
            children: vec![],
        });
        assert_eq!(or.estimate_candidates(&df), 903);
        assert_eq!(plan(PlanExpr::All).estimate_candidates(&df), 1000);
        assert_eq!(plan(PlanExpr::None).estimate_candidates(&df), 0);
    }

    #[test]
    fn tune_drops_stop_grams_but_keeps_a_discriminator() {
        let df = df_of(&[(b"the", 990), (b"ing", 900), (b"zqx", 2)], 1000);
        let mut plan = plan(PlanExpr::AllOf {
            grams: vec![
                GramNeedle::Key(key(b"the")),
                GramNeedle::Key(key(b"zqx")),
                GramNeedle::Key(key(b"ing")),
            ],
            needs: vec![],
            children: vec![],
        });
        plan.tune(&df, 500);
        let PlanExpr::AllOf { grams, .. } = plan.root() else {
            panic!("tuned plan must stay AllOf");
        };
        assert_eq!(grams.len(), 1);
        assert_eq!(grams[0], GramNeedle::Key(key(b"zqx")));
    }

    #[test]
    fn tune_keeps_the_rarest_stop_gram_when_all_are_stops() {
        let df = df_of(&[(b"the", 990), (b"ing", 900)], 1000);
        let mut plan = plan(PlanExpr::AllOf {
            grams: vec![GramNeedle::Key(key(b"the")), GramNeedle::Key(key(b"ing"))],
            needs: vec![],
            children: vec![],
        });
        plan.tune(&df, 500);
        let PlanExpr::AllOf { grams, .. } = plan.root() else {
            panic!("tuned plan must stay AllOf");
        };
        assert_eq!(grams.len(), 1);
        assert_eq!(grams[0], GramNeedle::Key(key(b"ing")));
    }

    #[test]
    fn tune_never_thins_or_bags() {
        let df = df_of(&[(b"the", 990), (b"zqx", 2)], 1000);
        let mut plan = plan(PlanExpr::AnyOf {
            grams: vec![GramNeedle::Key(key(b"the")), GramNeedle::Key(key(b"zqx"))],
            needs: vec![],
            children: vec![],
        });
        plan.tune(&df, 500);
        let PlanExpr::AnyOf { grams, .. } = plan.root() else {
            panic!("tuned plan must stay AnyOf");
        };
        assert_eq!(grams.len(), 2);
    }

    #[test]
    fn shapes_match_codesearch_regexp_test() {
        assert_shape("Abcdef", "G");
        assert_shape("(abc)(def)", "G");
        assert_shape("abc.*(def|ghi)", "(G & O & O)");
        assert_shape("a+hello", "G");
        assert_shape("(a+hello|b+world)", "(G | G)");
        assert_shape("a*bbb", "G");
        assert_shape("a?bbb", "G");
        assert_shape("(bbb)a?", "G");
        assert_shape("(bbb)a*", "G");
        assert_shape("^abc", "G");
        assert_shape("abc$", "G");
        assert_shape(r"[^\s\S]", "-");
        assert_shape("ab[^cde]f", "+");
        assert_shape("ab.f", "+");
        assert_shape(".", "+");
        assert_shape("()", "+");
        assert_shape("(abc|abc)", "G");
        assert_shape("(ab|ab)c", "G");
        assert_shape("ab[cd]e", "(G | G)");
        assert_shape("[ab][cd][ef]", "O");
        assert_shape("(?i)abc", "G");
        assert_shape("(?i)ab~", "G");
        assert_shape(r"\babc", "G");
    }
}
