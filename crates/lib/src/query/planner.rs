//! Query planning from regex HIR to public plans.

use regex_syntax::hir::Hir;
use sngram_types::{GramSpace, WeightTable};

use crate::{
    scan,
    types::{QueryError, QueryExpr, QueryPlan},
};

use super::{
    algebra::{Op, Query},
    analyze::{Analyzer, PlanContext},
    parser::QueryParser,
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
        let fold = scan::folded_space() && parsed.uses_folded_space();
        let ctx = PlanContext {
            fold,
            line_sentinels: scan::line_sentinels(),
        };
        Ok(QueryPlan::new(
            self.plan_hir(parsed.hir(), ctx),
            gram_space_for(fold),
        ))
    }

    fn plan_hir(&self, hir: &Hir, ctx: PlanContext) -> QueryExpr {
        let analyzer = Analyzer::with_context(self.table, ctx);
        into_public_expr(analyzer.plan(hir))
    }
}

const fn gram_space_for(fold: bool) -> GramSpace {
    if fold {
        GramSpace::Folded
    } else {
        GramSpace::Primary
    }
}

fn into_public_expr(query: Query) -> QueryExpr {
    match query.op {
        Op::All => QueryExpr::All,
        Op::None => QueryExpr::None,
        Op::And => QueryExpr::And {
            grams: query.grams.into_vec(),
            sub: query.sub.into_iter().map(into_public_expr).collect(),
        },
        Op::Or => QueryExpr::Or {
            grams: query.grams.into_vec(),
            sub: query.sub.into_iter().map(into_public_expr).collect(),
        },
    }
}

#[cfg(test)]
mod tests {
    //! Structure tests for regex query planning. End-to-end soundness lives in
    //! `tests/soundness.rs`.

    use std::collections::HashMap;

    use sngram_types::{Gram, GramSpace, WeightTable};

    use crate::{
        query::query,
        types::{DfStats, QueryError, QueryExpr, QueryPlan},
    };

    fn table() -> WeightTable {
        WeightTable::from_weight_fn(|c1, c2| crc32fast::hash(&[c1, c2]))
    }

    fn plan_of(re: &str) -> QueryPlan {
        query(&table(), re).expect("pattern parses")
    }

    fn expr_of(re: &str) -> QueryExpr {
        plan_of(re).expr().clone()
    }

    fn shape(expr: &QueryExpr) -> String {
        match expr {
            QueryExpr::All => "+".to_string(),
            QueryExpr::None => "-".to_string(),
            QueryExpr::And { grams, sub } => shape_join(grams, sub, "G", " & "),
            QueryExpr::Or { grams, sub } => shape_join(grams, sub, "O", " | "),
        }
    }

    fn shape_join(grams: &[Gram], sub: &[QueryExpr], bag: &str, sep: &str) -> String {
        let mut parts = Vec::new();
        if !grams.is_empty() {
            parts.push(bag.to_string());
        }
        parts.extend(sub.iter().map(shape));
        if parts.len() == 1 {
            return parts.pop().expect("len 1");
        }
        format!("({})", parts.join(sep))
    }

    fn has_or(expr: &QueryExpr) -> bool {
        match expr {
            QueryExpr::Or { .. } => true,
            QueryExpr::And { sub, .. } => sub.iter().any(has_or),
            _ => false,
        }
    }

    fn assert_shape(re: &str, expected: &str) {
        assert_eq!(shape(&expr_of(re)), expected, "shape mismatch for {re:?}");
    }

    #[test]
    fn literal_pattern_extracts_grams() {
        assert!(matches!(expr_of("MAX_FILE_SIZE"), QueryExpr::And { .. }));
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
        assert!(matches!(expr_of(r"ab\bc"), QueryExpr::None));
        assert!(matches!(expr_of(r"foo$bar"), QueryExpr::None));
    }

    #[test]
    fn alternation_and_repetition_keep_selective_constraints() {
        assert!(matches!(expr_of("(a+hello|b+world)"), QueryExpr::Or { .. }));
        assert!(!expr_of("a{3,5}bcdef").is_all());
        assert!(!expr_of("foo[α-γ]bar").is_all());
        assert_eq!(expr_of("ab[cd]ef"), expr_of("abcef|abdef"));
        assert_eq!(expr_of("x{5}"), expr_of("xxxxx"));
        assert_eq!(expr_of("h{3,5}i"), expr_of("hhhi|hhhhi|hhhhhi"));
    }

    #[test]
    fn inline_case_insensitive_uses_folded_space() {
        let plan = plan_of("(?i)netif_receive_skb_list_internal");
        assert_eq!(plan.space(), GramSpace::Folded);
        assert!(plan.gram_count() > 0);
    }

    #[test]
    fn sensitive_queries_use_primary_space() {
        let plan = plan_of("SchedClock");
        assert_eq!(plan.space(), GramSpace::Primary);
    }

    #[test]
    fn folded_plans_never_carry_uppercase_ascii() {
        let plan = plan_of("(?i:READ[A-Z]lock_IRQ)");
        assert_eq!(plan.space(), GramSpace::Folded);
        for gram in grams_of(plan.expr()) {
            assert!(
                !gram.as_bytes().iter().any(u8::is_ascii_uppercase),
                "uppercase byte in folded-space gram {gram:?}"
            );
        }
    }

    fn grams_of(expr: &QueryExpr) -> Vec<Gram> {
        let mut out = Vec::new();
        collect_grams(expr, &mut out);
        out
    }

    fn collect_grams(expr: &QueryExpr, out: &mut Vec<Gram>) {
        let (QueryExpr::And { grams, sub } | QueryExpr::Or { grams, sub }) = expr else {
            return;
        };
        out.extend(grams.iter().cloned());
        for child in sub {
            collect_grams(child, out);
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
        counts: HashMap<Vec<u8>, u64>,
        total: u64,
    }

    impl DfStats for MapDf {
        fn doc_count(&self, _space: GramSpace, gram: &Gram) -> u64 {
            self.counts.get(gram.as_bytes()).copied().unwrap_or(0)
        }

        fn total_docs(&self) -> u64 {
            self.total
        }
    }

    fn df_of(pairs: &[(&[u8], u64)], total: u64) -> MapDf {
        MapDf {
            counts: pairs.iter().map(|(gram, n)| (gram.to_vec(), *n)).collect(),
            total,
        }
    }

    fn primary(expr: QueryExpr) -> QueryPlan {
        QueryPlan::new(expr, GramSpace::Primary)
    }

    #[test]
    fn estimate_bounds_and_by_rarest_and_or_by_sum() {
        let and = primary(QueryExpr::And {
            grams: vec![Gram::from(&b"abc"[..]), Gram::from(&b"xyz"[..])],
            sub: vec![],
        });
        let df = df_of(&[(b"abc", 900), (b"xyz", 3)], 1000);
        assert_eq!(and.estimate_candidates(&df), 3);

        let or = primary(QueryExpr::Or {
            grams: vec![Gram::from(&b"abc"[..]), Gram::from(&b"xyz"[..])],
            sub: vec![],
        });
        assert_eq!(or.estimate_candidates(&df), 903);
        assert_eq!(primary(QueryExpr::All).estimate_candidates(&df), 1000);
        assert_eq!(primary(QueryExpr::None).estimate_candidates(&df), 0);
    }

    #[test]
    fn tune_drops_stop_grams_but_keeps_a_discriminator() {
        let df = df_of(&[(b"the", 990), (b"ing", 900), (b"zqx", 2)], 1000);
        let mut plan = primary(QueryExpr::And {
            grams: vec![
                Gram::from(&b"the"[..]),
                Gram::from(&b"zqx"[..]),
                Gram::from(&b"ing"[..]),
            ],
            sub: vec![],
        });
        plan.tune(&df, 500);
        let QueryExpr::And { grams, .. } = plan.expr() else {
            panic!("tuned plan must stay And");
        };
        assert_eq!(grams.len(), 1);
        assert_eq!(grams[0].as_bytes(), b"zqx");
    }

    #[test]
    fn tune_keeps_the_rarest_stop_gram_when_all_are_stops() {
        let df = df_of(&[(b"the", 990), (b"ing", 900)], 1000);
        let mut plan = primary(QueryExpr::And {
            grams: vec![Gram::from(&b"the"[..]), Gram::from(&b"ing"[..])],
            sub: vec![],
        });
        plan.tune(&df, 500);
        let QueryExpr::And { grams, .. } = plan.expr() else {
            panic!("tuned plan must stay And");
        };
        assert_eq!(grams.len(), 1);
        assert_eq!(grams[0].as_bytes(), b"ing");
    }

    #[test]
    fn tune_never_thins_or_bags() {
        let df = df_of(&[(b"the", 990), (b"zqx", 2)], 1000);
        let mut plan = primary(QueryExpr::Or {
            grams: vec![Gram::from(&b"the"[..]), Gram::from(&b"zqx"[..])],
            sub: vec![],
        });
        plan.tune(&df, 500);
        let QueryExpr::Or { grams, .. } = plan.expr() else {
            panic!("tuned plan must stay Or");
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
