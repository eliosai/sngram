//! Query planning from regex HIR to public plans.

use regex_syntax::hir::{Hir, HirKind, Look};
use sngram_types::{
    Gram, GramKey, GramNeedle, HashKey, PlanExpr, QueryError, QueryPlan, ScanNeed, WeightTable,
};

use super::{
    algebra::{Op, Query},
    analyze::{Analyzer, PlanContext, is_word_byte},
    needs::RootNeeds,
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
        let edges = if ctx.fold {
            None
        } else {
            word_edged_literal(hir)
        };
        with_root_needs(
            into_public_expr(analyzer.plan(hir), ctx.fold, edges),
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
    match expr {
        PlanExpr::All => PlanExpr::AllOf {
            grams: vec![],
            needs: new_needs,
            children: vec![],
        },
        PlanExpr::AllOf {
            grams,
            mut needs,
            children,
        } => {
            needs.extend(new_needs);
            PlanExpr::AllOf {
                grams,
                needs,
                children,
            }
        },
        other => PlanExpr::AllOf {
            grams: vec![],
            needs: new_needs,
            children: vec![other],
        },
    }
}

fn into_public_expr(query: Query, fold: bool, edges: Option<&[u8]>) -> PlanExpr {
    match query.op {
        Op::All => PlanExpr::All,
        Op::None => PlanExpr::None,
        Op::And => PlanExpr::AllOf {
            grams: public_grams(query.grams, fold, edges),
            needs: Vec::new(),
            children: public_children(query.sub, fold, edges),
        },
        Op::Or => PlanExpr::AnyOf {
            grams: public_grams(query.grams, fold, edges),
            needs: Vec::new(),
            children: public_children(query.sub, fold, edges),
        },
    }
}

fn public_grams(grams: StringSet, fold: bool, edges: Option<&[u8]>) -> Vec<GramNeedle> {
    grams
        .into_vec()
        .into_iter()
        .map(|gram| needle_for(&gram, fold, edges))
        .collect()
}

fn public_children(children: Vec<Query>, fold: bool, edges: Option<&[u8]>) -> Vec<PlanExpr> {
    children
        .into_iter()
        .map(|query| into_public_expr(query, fold, edges))
        .collect()
}

fn needle_for(gram: &Gram, fold: bool, edges: Option<&[u8]>) -> GramNeedle {
    let raw = GramKey(HashKey::UNKEYED.hash_bytes(gram.as_bytes()));
    let keys = if !fold || !gram.as_bytes().iter().any(u8::is_ascii_alphabetic) {
        vec![raw]
    } else {
        vec![
            raw,
            GramKey(HashKey::UNKEYED.folded().hash_bytes(gram.as_bytes())),
        ]
    };
    if let Some(literal) = edges {
        let starts = literal.starts_with(gram.as_bytes());
        let ends = literal.ends_with(gram.as_bytes());
        if starts || ends {
            return GramNeedle::AtWordEdge { keys, starts, ends };
        }
    }
    if keys.len() == 1 {
        GramNeedle::Key(raw)
    } else {
        GramNeedle::AnyKey(keys)
    }
}

/// The literal of a whole-pattern `\b literal \b` shape whose word-byte
/// edges make gram occurrences at the literal's edges word-bounded
fn word_edged_literal(hir: &Hir) -> Option<&[u8]> {
    let HirKind::Concat(subs) = hir.kind() else {
        return None;
    };
    let [first, mid, last] = subs.as_slice() else {
        return None;
    };
    if !is_word_look(first) || !is_word_look(last) {
        return None;
    }
    let HirKind::Literal(lit) = unwrap_captures(mid).kind() else {
        return None;
    };
    let (head, tail) = (*lit.0.first()?, *lit.0.last()?);
    (is_word_byte(head) && is_word_byte(tail)).then_some(&lit.0)
}

fn is_word_look(hir: &Hir) -> bool {
    matches!(
        hir.kind(),
        HirKind::Look(Look::WordAscii | Look::WordUnicode)
    )
}

fn unwrap_captures(hir: &Hir) -> &Hir {
    match hir.kind() {
        HirKind::Capture(capture) => unwrap_captures(&capture.sub),
        _ => hir,
    }
}

#[cfg(test)]
mod tests {
    //! Structure tests for regex query planning. End-to-end soundness lives in
    //! `tests/soundness.rs`.

    use std::collections::HashMap;

    use sngram_types::{
        DfStats, GramKey, GramNeedle, HashKey, PlanExpr, QueryError, QueryPlan, ScanNeed,
        WeightTable,
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
        if parts.is_empty() {
            return "+".to_string();
        }
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
        assert!(has_min_byte_len(plan_of(r"[a-z]+").root(), 1));
        assert!(has_min_byte_len(plan_of("ab").root(), 2));
    }

    #[test]
    fn planner_emits_minimum_byte_length_need() {
        assert!(has_min_byte_len(plan_of(".").root(), 1));
        assert!(has_min_byte_len(plan_of("abc").root(), 3));
        assert!(has_min_byte_len(plan_of("abc|de").root(), 2));
    }

    #[test]
    fn planner_emits_required_byte_counts() {
        let repeated = plan_of("(ab){5}");
        assert!(has_min_byte_count(repeated.root(), b'a', 5));
        assert!(has_min_byte_count(repeated.root(), b'b', 5));

        let version = plan_of(r"v[0-9]+\.[0-9]+\.[0-9]+");
        assert!(has_min_byte_count(version.root(), b'.', 2));

        let branch = plan_of("abcef|abdef");
        for byte in b"abef" {
            assert!(has_min_byte_count(branch.root(), *byte, 1));
        }
        assert!(!has_min_byte_count(branch.root(), b'c', 1));
        assert!(!has_min_byte_count(branch.root(), b'd', 1));
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
        assert!(has_or(&expr_of("(a+hello|b+world)")));
        assert!(!expr_of("a{3,5}bcdef").is_all());
        assert!(!expr_of("foo[α-γ]bar").is_all());
        assert_eq!(
            without_root_needs(expr_of("ab[cd]ef")),
            without_root_needs(expr_of("abcef|abdef"))
        );
        assert_eq!(expr_of("x{5}"), expr_of("xxxxx"));
        assert_eq!(expr_of("h{3,5}i"), expr_of("hhhi|hhhhi|hhhhhi"));
    }

    fn without_root_needs(expr: PlanExpr) -> PlanExpr {
        match expr {
            PlanExpr::AllOf {
                grams, children, ..
            } => PlanExpr::AllOf {
                grams,
                needs: Vec::new(),
                children,
            },
            other => other,
        }
    }

    #[test]
    fn inline_case_insensitive_uses_key_alternatives() {
        let plan = plan_of("(?i)netif_receive_skb_list_internal");
        assert!(plan.gram_count() > 0);
        assert!(has_any_key(plan.root()));
    }

    #[test]
    fn word_bounded_literal_lowers_to_word_edged_needles() {
        let plan = plan_of(r"\bmain\b");
        let (mut starts, mut ends) = (false, false);
        each_needle(plan.root(), &mut |needle| {
            if let GramNeedle::AtWordEdge {
                starts: s, ends: e, ..
            } = needle
            {
                starts |= s;
                ends |= e;
            }
        });
        assert!(starts && ends, "expected word-edged needles in {plan}");

        let unbounded = plan_of("main");
        each_needle(unbounded.root(), &mut |needle| {
            assert!(!matches!(needle, GramNeedle::AtWordEdge { .. }));
        });
    }

    fn each_needle(expr: &PlanExpr, visit: &mut impl FnMut(&GramNeedle)) {
        if let PlanExpr::AllOf {
            grams, children, ..
        }
        | PlanExpr::AnyOf {
            grams, children, ..
        } = expr
        {
            for gram in grams {
                visit(gram);
            }
            for child in children {
                each_needle(child, visit);
            }
        }
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

    fn has_min_byte_len(expr: &PlanExpr, len: u64) -> bool {
        match expr {
            PlanExpr::All | PlanExpr::None => false,
            PlanExpr::AllOf {
                needs, children, ..
            }
            | PlanExpr::AnyOf {
                needs, children, ..
            } => {
                needs.contains(&ScanNeed::MinByteLen(len))
                    || children.iter().any(|child| has_min_byte_len(child, len))
            },
        }
    }

    fn has_min_byte_count(expr: &PlanExpr, byte: u8, count: u8) -> bool {
        match expr {
            PlanExpr::All | PlanExpr::None => false,
            PlanExpr::AllOf {
                needs, children, ..
            }
            | PlanExpr::AnyOf {
                needs, children, ..
            } => {
                needs.iter().any(|need| {
                    matches!(
                        need,
                        ScanNeed::MinByteCounts(counts)
                            if counts.counts[usize::from(byte)] >= count
                    )
                }) || children
                    .iter()
                    .any(|child| has_min_byte_count(child, byte, count))
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
        assert_eq!(plan_of(".").to_string(), "MinByteLen(1)");
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
