//! Regex to sparse-gram query analysis.
//!
//! A regex is folded bottom-up into a [`QueryPlan`]: a conservative boolean
//! query over gram presence that every matching document must satisfy. The
//! plan over-approximates (false positives are fine; a real match is never
//! dropped), so it prefilters candidates before the exact regex runs.

mod analyze;
mod combine;
mod info;
mod options;
mod query;
mod strings;

use regex_syntax::hir::Hir;
#[cfg(test)]
use sngram_types::Gram;
use sngram_types::WeightTable;

use crate::types::{MAX_PATTERN_LEN, QueryError};
use analyze::{Analyzer, PlanContext};
use query::{Op, Query};

pub use crate::types::{GramSpace, PlannedQuery, QueryCase, QueryOptions, QueryPlan, QuerySyntax};

/// Nest limit matching `grep-regex`'s translator, so any pattern the
/// verifier accepts also parses here.
const VERIFIER_NEST_LIMIT: u32 = 250;

/// Decompose one or more patterns under the verifier's match options.
///
/// The patterns are escaped (for fixed strings), OR-joined, and parsed with
/// the same flags the verifying engine uses — including engine-rule smart
/// case — so the plan can never assume narrower semantics than the match
/// run that follows. Inversion yields [`QueryPlan::All`]: every document may
/// hold a non-matching line.
pub fn query<P: AsRef<str>>(
    table: &WeightTable,
    patterns: &[P],
    opts: QueryOptions,
) -> Result<PlannedQuery, QueryError> {
    if opts.invert || patterns.is_empty() {
        return Ok(PlannedQuery {
            plan: QueryPlan::All,
            space: GramSpace::Primary,
        });
    }
    let joined = join_patterns(patterns, opts)?;
    let global_insensitive = effective_insensitive(&joined, opts);
    let inline_insensitive =
        matches!(opts.syntax, QuerySyntax::Regex) && options::has_inline_case_insensitive(&joined);
    let fold = opts.folded_space && (global_insensitive || inline_insensitive);
    let hir = parse_joined(&joined, opts)?;
    let ctx = PlanContext {
        fold,
        line_sentinels: opts.line_sentinels,
    };
    Ok(PlannedQuery {
        plan: query_hir_ctx(table, &hir, ctx),
        space: if fold {
            GramSpace::Folded
        } else {
            GramSpace::Primary
        },
    })
}

/// Escape (for fixed strings), wrap, and OR-join the patterns.
fn join_patterns<P: AsRef<str>>(patterns: &[P], opts: QueryOptions) -> Result<String, QueryError> {
    let mut parts = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        let pattern = pattern.as_ref();
        match opts.syntax {
            QuerySyntax::Regex => parts.push(format!("(?:{pattern})")),
            QuerySyntax::FixedStrings => {
                parts.push(format!("(?:{})", regex_syntax::escape(pattern)));
            },
        }
    }
    let joined = parts.join("|");
    check_length(&joined)?;
    Ok(joined)
}

const fn check_length(regex: &str) -> Result<(), QueryError> {
    if regex.len() > MAX_PATTERN_LEN {
        return Err(QueryError::PatternTooLong {
            len: regex.len(),
            max: MAX_PATTERN_LEN,
        });
    }
    Ok(())
}

/// Whether the verifier will match `joined` case-insensitively.
fn effective_insensitive(joined: &str, opts: QueryOptions) -> bool {
    match opts.case {
        QueryCase::Sensitive => false,
        QueryCase::Insensitive => true,
        QueryCase::Smart => options::smart_case_insensitive(joined),
    }
}

/// Parse the joined pattern with the flags the verifying engine uses.
fn parse_joined(joined: &str, opts: QueryOptions) -> Result<Hir, QueryError> {
    let insensitive = effective_insensitive(joined, opts);
    regex_syntax::ParserBuilder::new()
        .nest_limit(VERIFIER_NEST_LIMIT)
        .octal(false)
        .utf8(false)
        // Always on, like grep-regex's translator. Sound for any verifier:
        // line anchors are strictly harder to prove impossible than text
        // anchors, so None-pruning never over-fires.
        .multi_line(true)
        .case_insensitive(insensitive)
        .dot_matches_new_line(opts.dotall)
        .crlf(opts.crlf)
        .unicode(opts.unicode)
        .build()
        .parse(joined)
        .map_err(|err| QueryError::InvalidRegex(Box::new(err)))
}

/// Fold an already-parsed HIR into a plan under an index-format context.
fn query_hir_ctx(table: &WeightTable, hir: &Hir, ctx: PlanContext) -> QueryPlan {
    let analyzer = Analyzer::with_context(table, ctx);
    let mut info = analyzer.analyze(hir);
    analyzer.begin_final_flush();
    analyzer.simplify(&mut info, true);
    analyzer.add_exact(&mut info);
    to_plan(info.match_)
}

/// Convert the internal query into the public plan. The internal `Query` is
/// already codesearch's `{op, grams, sub}`, so this is a direct structural map.
fn to_plan(q: Query) -> QueryPlan {
    match q.op {
        Op::All => QueryPlan::All,
        Op::None => QueryPlan::None,
        Op::And => QueryPlan::And {
            grams: q.grams.into_vec(),
            sub: q.sub.into_iter().map(to_plan).collect(),
        },
        Op::Or => QueryPlan::Or {
            grams: q.grams.into_vec(),
            sub: q.sub.into_iter().map(to_plan).collect(),
        },
    }
}

#[cfg(test)]
mod tests {
    //! Structure tests: the boolean shape a regex folds to, compared to Google
    //! codesearch `regexp_test.go` (operator structure, not the trigram strings,
    //! which differ for sparse grams). End-to-end soundness is in
    //! `tests/soundness.rs`.

    use sngram_types::WeightTable;

    use super::{QueryOptions, QueryPlan, query};

    /// A deterministic weight table: each byte pair hashed to a varied weight,
    /// so the sparse hull is non-trivial.
    fn table() -> WeightTable {
        WeightTable::from_weight_fn(|c1, c2| crc32fast::hash(&[c1, c2]))
    }

    fn plan_of(re: &str) -> QueryPlan {
        query(&table(), &[re], QueryOptions::default())
            .expect("pattern parses")
            .plan
    }

    /// Render the operator tree, collapsing a conjunctive gram bag to `G` and a
    /// disjunctive bag to `O`, so the shape compares to codesearch independent
    /// of sparse-vs-trigram gram counts. An `Or` of grams is one overlap op,
    /// exactly codesearch's `("a"|"b")` single `QOr`.
    fn shape(plan: &QueryPlan) -> String {
        match plan {
            QueryPlan::All => "+".to_string(),
            QueryPlan::None => "-".to_string(),
            QueryPlan::And { grams, sub } => shape_join(grams, sub, "G", " & "),
            QueryPlan::Or { grams, sub } => shape_join(grams, sub, "O", " | "),
        }
    }

    fn shape_join(grams: &[super::Gram], sub: &[QueryPlan], bag: &str, sep: &str) -> String {
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

    fn has_or(plan: &QueryPlan) -> bool {
        match plan {
            QueryPlan::Or { .. } => true,
            QueryPlan::And { sub, .. } => sub.iter().any(has_or),
            _ => false,
        }
    }

    fn assert_shape(re: &str, expected: &str) {
        assert_eq!(shape(&plan_of(re)), expected, "shape mismatch for {re:?}");
    }

    #[test]
    fn anchors_and_word_boundaries_are_invisible() {
        assert_eq!(plan_of("^abc"), plan_of("abc"));
        assert_eq!(plan_of("abc$"), plan_of("abc"));
        assert_eq!(plan_of(r"\babc"), plan_of("abc"));
        // A satisfiable interior boundary adds nothing to the plan; an
        // unsatisfiable one (like `ab\bc`) proves the pattern matches
        // nothing and plans to None instead.
        assert_eq!(plan_of(r"ab\b-cd"), plan_of("ab-cd"));
        assert_eq!(plan_of(r"ab\bc"), QueryPlan::None);
    }

    #[test]
    fn capture_group_is_transparent() {
        assert_eq!(plan_of("(abcdef)"), plan_of("abcdef"));
    }

    #[test]
    fn alternation_of_literals_is_an_or() {
        assert!(matches!(plan_of("(a+hello|b+world)"), QueryPlan::Or { .. }));
    }

    #[test]
    fn case_insensitive_expands_to_an_or_over_variants() {
        assert!(matches!(plan_of("(?i)abc"), QueryPlan::Or { .. }));
    }

    #[test]
    fn impossible_pattern_is_none() {
        assert_eq!(plan_of(r"[^\s\S]"), QueryPlan::None);
    }

    #[test]
    fn too_broad_patterns_are_all() {
        assert_eq!(plan_of("."), QueryPlan::All);
        assert_eq!(plan_of("()"), QueryPlan::All);
        assert_eq!(plan_of("ab[^cde]f"), QueryPlan::All);
    }

    #[test]
    fn counted_repetition_keeps_the_literal() {
        assert!(!matches!(plan_of("a{3,5}bcdef"), QueryPlan::All));
    }

    #[test]
    fn small_unicode_class_stays_a_constraint() {
        assert!(!matches!(plan_of("foo[α-γ]bar"), QueryPlan::All));
    }

    #[test]
    fn small_class_between_literals_matches_explicit_alternation() {
        assert_eq!(plan_of("ab[cd]ef"), plan_of("abcef|abdef"));
        assert_eq!(
            plan_of("sched[_-]clock"),
            plan_of("sched_clock|sched-clock")
        );
    }

    #[test]
    fn exact_base_repetition_above_cap_expands_to_literal() {
        // A counted repetition whose base is a small exact set expands to its
        // exact language even above MAX_REPEAT_EXPAND, so it plans identically
        // to the fully written-out literal instead of a demoted stub.
        assert_eq!(plan_of("x{5}"), plan_of("xxxxx"));
        assert_eq!(plan_of("ab{5}cd"), plan_of("abbbbbcd"));
        assert_eq!(plan_of("(abc){5}"), plan_of("abcabcabcabcabc"));
        assert_eq!(plan_of("a{8}"), plan_of("aaaaaaaa"));
    }

    #[test]
    fn bounded_range_above_cap_matches_enumeration() {
        // A narrow range above the cap enumerates every allowed count exactly,
        // so `h{3,5}i` is `(hhh|hhhh|hhhhh)i` and `[ch]{5}` is the exact set of
        // all 32 length-5 strings over {c,h}.
        assert_eq!(plan_of("h{3,5}i"), plan_of("hhhi|hhhhi|hhhhhi"));
        assert_eq!(plan_of("[ch]{5}"), plan_of("(?:c|h){5}"));
    }

    #[test]
    fn exact_literal_cover_keeps_dense_subwindows() {
        let plan = plan_of("sched_clock");
        let rendered = plan.to_string();
        for gram in [
            "sch", "che", "hed", "ed_", "d_c", "_cl", "clo", "loc", "ock",
        ] {
            assert!(
                rendered.contains(gram),
                "expected {rendered:?} to contain dense literal gram {gram:?}"
            );
        }
    }

    #[test]
    fn nested_alternations_both_survive() {
        let plan = plan_of("(z*abcz*defz*)(z*(ghi|jkl)z*)");
        assert!(has_or(&plan), "alternation lost in {}", shape(&plan));
        assert!(
            shape(&plan).contains('&'),
            "concat lost in {}",
            shape(&plan)
        );
    }

    #[test]
    fn display_matches_codesearch_string_forms() {
        assert_eq!(plan_of(".").to_string(), "+");
        assert_eq!(plan_of(r"[^\s\S]").to_string(), "-");
        assert!(plan_of("(a+hello|b+world)").to_string().contains('|'));
    }

    mod options_spec {
        //! `query` must build plans from exactly the verifier's
        //! semantics: engine-rule smart case, fixed-string escaping,
        //! OR-joined multiple patterns, and All for inversion.

        use super::{plan_of, table};
        use crate::plan::{QueryCase, QueryOptions, QueryPlan, QuerySyntax, query};

        fn with(opts: QueryOptions, patterns: &[&str]) -> QueryPlan {
            query(&table(), patterns, opts).expect("patterns plan").plan
        }

        #[test]
        fn smart_case_folds_when_uppercase_only_in_escapes() {
            // grep-regex treats \W as class, not literal: the verifier
            // matches insensitively, so the plan must too.
            let smart = with(
                QueryOptions {
                    case: QueryCase::Smart,
                    ..QueryOptions::default()
                },
                &[r"maxfile\Wsize"],
            );
            let insensitive = with(
                QueryOptions {
                    case: QueryCase::Insensitive,
                    ..QueryOptions::default()
                },
                &[r"maxfile\Wsize"],
            );
            assert_eq!(smart, insensitive);
        }

        #[test]
        fn smart_case_stays_sensitive_on_uppercase_literals() {
            let smart = with(
                QueryOptions {
                    case: QueryCase::Smart,
                    ..QueryOptions::default()
                },
                &["MaxFile"],
            );
            let sensitive = with(QueryOptions::default(), &["MaxFile"]);
            assert_eq!(smart, sensitive);
        }

        #[test]
        fn fixed_strings_escape_metacharacters() {
            let fixed = with(
                QueryOptions {
                    syntax: QuerySyntax::FixedStrings,
                    ..QueryOptions::default()
                },
                &["max.*size"],
            );
            assert_eq!(fixed, plan_of(r"max\.\*size"));
        }

        #[test]
        fn invert_plans_everything() {
            let inverted = with(
                QueryOptions {
                    invert: true,
                    ..QueryOptions::default()
                },
                &["max_file_size"],
            );
            assert_eq!(inverted, QueryPlan::All);
        }

        #[test]
        fn multiple_patterns_join_as_alternation() {
            let joined = with(QueryOptions::default(), &["max_file", "min_file"]);
            assert_eq!(joined, plan_of("(?:max_file)|(?:min_file)"));
        }

        #[test]
        fn insensitive_flag_equals_inline_wrapping() {
            // eg used to wrap patterns as (?i:..) before parsing; the flag
            // path must plan identically.
            for pat in ["max_file_size", "sched[_-]clock", "mem+set"] {
                let flagged = with(
                    QueryOptions {
                        case: QueryCase::Insensitive,
                        ..QueryOptions::default()
                    },
                    &[pat],
                );
                assert_eq!(flagged, plan_of(&format!("(?i:{pat})")), "{pat}");
            }
        }

        #[test]
        fn crlf_keeps_carriage_return_anchors_satisfiable() {
            // Without crlf, $ is EndLF and a following \r proves the
            // pattern empty; with crlf the anchor accepts \r and the plan
            // must survive. Mis-plumbing this field would silently drop
            // real matches.
            let default = with(QueryOptions::default(), &[r"foo$\r\nbar"]);
            assert_eq!(default, QueryPlan::None);
            let crlf = with(
                QueryOptions {
                    crlf: true,
                    ..QueryOptions::default()
                },
                &[r"foo$\r\nbar"],
            );
            assert!(!matches!(crlf, QueryPlan::None));
        }

        #[test]
        fn non_unicode_byte_patterns_parse() {
            let opts = QueryOptions {
                unicode: false,
                ..QueryOptions::default()
            };
            let plan = query(&table(), &[r"foobar\xFF"], opts)
                .expect("bytes plan")
                .plan;
            assert!(!matches!(plan, QueryPlan::All));
        }
    }

    mod format_spec {
        //! `query` under an index format: sentinel anchors and the folded
        //! space, each gated on the index actually carrying them.

        use super::table;
        use crate::plan::{GramSpace, QueryCase, QueryOptions, QueryPlan, query};
        use crate::types::DfStats;
        use sngram_types::Gram;

        fn planned(patterns: &[&str], opts: QueryOptions) -> super::QueryPlan {
            query(&table(), patterns, opts).expect("plan").plan
        }

        fn sentinels(opts: QueryOptions) -> QueryOptions {
            QueryOptions {
                line_sentinels: true,
                ..opts
            }
        }

        fn folded(opts: QueryOptions) -> QueryOptions {
            QueryOptions {
                folded_space: true,
                ..opts
            }
        }

        fn grams_of(plan: &QueryPlan, out: &mut Vec<Gram>) {
            let (QueryPlan::And { grams, sub } | QueryPlan::Or { grams, sub }) = plan else {
                return;
            };
            out.extend(grams.iter().cloned());
            for p in sub {
                grams_of(p, out);
            }
        }

        fn all_grams(plan: &QueryPlan) -> Vec<Gram> {
            let mut out = Vec::new();
            grams_of(plan, &mut out);
            out
        }

        #[test]
        fn start_anchor_demands_terminator_grams() {
            let plan = planned(&["^#define CONFIG"], sentinels(QueryOptions::default()));
            assert!(
                all_grams(&plan)
                    .iter()
                    .any(|g| g.as_bytes().first() == Some(&b'\n')),
                "expected a leading terminator-bridging gram in {plan}"
            );
        }

        #[test]
        fn end_anchor_demands_terminator_grams() {
            let plan = planned(&["EXPORT_SYMBOL$"], sentinels(QueryOptions::default()));
            assert!(
                all_grams(&plan)
                    .iter()
                    .any(|g| g.as_bytes().last() == Some(&b'\n')),
                "expected a trailing terminator-bridging gram in {plan}"
            );
        }

        #[test]
        fn anchors_without_sentinels_stay_invisible() {
            let anchored = planned(&["^abcdef"], QueryOptions::default());
            let plain = planned(&["abcdef"], QueryOptions::default());
            assert_eq!(anchored, plain);
        }

        #[test]
        fn crlf_end_anchor_covers_both_terminators() {
            let opts = QueryOptions {
                crlf: true,
                ..QueryOptions::default()
            };
            let plan = planned(&["EXPORT_SYMBOL$"], sentinels(opts));
            let grams = all_grams(&plan);
            assert!(grams.iter().any(|g| g.as_bytes().last() == Some(&b'\n')));
            assert!(grams.iter().any(|g| g.as_bytes().last() == Some(&b'\r')));
        }

        #[test]
        fn interior_anchor_pruning_survives_sentinels() {
            let plan = planned(&["foo$bar"], sentinels(QueryOptions::default()));
            assert_eq!(
                plan,
                QueryPlan::None,
                "impossible interior anchor must stay None"
            );
        }

        #[test]
        fn ascii_insensitive_folds_to_the_lowercase_plan() {
            let opts = QueryOptions {
                case: QueryCase::Insensitive,
                unicode: false,
                ..QueryOptions::default()
            };
            let folded_plan = query(&table(), &["SchedClock"], folded(opts)).expect("plan");
            assert_eq!(folded_plan.space, GramSpace::Folded);
            let lower = query(
                &table(),
                &["schedclock"],
                QueryOptions {
                    unicode: false,
                    ..QueryOptions::default()
                },
            )
            .expect("plan");
            assert_eq!(
                folded_plan.plan, lower.plan,
                "folded plan must equal the folded literal's"
            );
        }

        #[test]
        fn unicode_insensitive_collapses_the_ascii_explosion() {
            let opts = QueryOptions {
                case: QueryCase::Insensitive,
                ..QueryOptions::default()
            };
            let folded_plan = query(&table(), &["SchedClock"], folded(opts)).expect("plan");
            assert_eq!(folded_plan.space, GramSpace::Folded);
            let exploded = query(&table(), &["SchedClock"], opts).expect("plan");
            let folded_count = all_grams(&folded_plan.plan).len();
            let exploded_count = all_grams(&exploded.plan).len();
            assert!(
                folded_count < exploded_count,
                "folded plan ({folded_count} grams) must be smaller than the variant explosion ({exploded_count})"
            );
        }

        #[test]
        fn inline_insensitive_uses_folded_space() {
            let opts = QueryOptions::default();
            let pattern = "(?i)netif_receive_skb_list_internal";
            let folded_plan = query(&table(), &[pattern], folded(opts)).expect("plan");
            assert_eq!(folded_plan.space, GramSpace::Folded);
            let exploded = query(&table(), &[pattern], opts).expect("exploded plan");
            let folded_count = all_grams(&folded_plan.plan).len();
            let exploded_count = all_grams(&exploded.plan).len();
            assert!(
                folded_count.saturating_mul(4) < exploded_count,
                "folded inline plan ({folded_count} grams) should avoid primary-space explosion ({exploded_count})"
            );
        }

        #[test]
        fn folded_plans_never_carry_uppercase() {
            let opts = QueryOptions {
                case: QueryCase::Insensitive,
                ..QueryOptions::default()
            };
            let planned = query(&table(), &["READ[A-Z]lock_IRQ"], folded(opts)).expect("plan");
            assert_eq!(planned.space, GramSpace::Folded);
            for g in all_grams(&planned.plan) {
                assert!(
                    !g.as_bytes().iter().any(u8::is_ascii_uppercase),
                    "uppercase byte in folded-space gram {g:?}"
                );
            }
        }

        #[test]
        fn sensitive_queries_ignore_the_folded_space() {
            let planned =
                query(&table(), &["SchedClock"], folded(QueryOptions::default())).expect("plan");
            assert_eq!(planned.space, GramSpace::Primary);
        }

        struct MapDf {
            counts: std::collections::HashMap<Vec<u8>, u64>,
            total: u64,
        }

        impl DfStats for MapDf {
            fn doc_count(&self, gram: &Gram) -> u64 {
                self.counts.get(gram.as_bytes()).copied().unwrap_or(0)
            }

            fn total_docs(&self) -> u64 {
                self.total
            }
        }

        fn df_of(pairs: &[(&[u8], u64)], total: u64) -> MapDf {
            MapDf {
                counts: pairs.iter().map(|(g, n)| (g.to_vec(), *n)).collect(),
                total,
            }
        }

        #[test]
        fn estimate_bounds_and_by_rarest_and_or_by_sum() {
            let and = QueryPlan::And {
                grams: vec![Gram::from(&b"abc"[..]), Gram::from(&b"xyz"[..])],
                sub: vec![],
            };
            let df = df_of(&[(b"abc", 900), (b"xyz", 3)], 1000);
            assert_eq!(and.estimate_candidates(&df), 3);

            let or = QueryPlan::Or {
                grams: vec![Gram::from(&b"abc"[..]), Gram::from(&b"xyz"[..])],
                sub: vec![],
            };
            assert_eq!(or.estimate_candidates(&df), 903);
            assert_eq!(QueryPlan::All.estimate_candidates(&df), 1000);
            assert_eq!(QueryPlan::None.estimate_candidates(&df), 0);
        }

        #[test]
        fn tune_drops_stop_grams_but_keeps_a_discriminator() {
            let df = df_of(&[(b"the", 990), (b"ing", 900), (b"zqx", 2)], 1000);
            let mut plan = QueryPlan::And {
                grams: vec![
                    Gram::from(&b"the"[..]),
                    Gram::from(&b"zqx"[..]),
                    Gram::from(&b"ing"[..]),
                ],
                sub: vec![],
            };
            plan.tune(&df, 500);
            let QueryPlan::And { grams, .. } = &plan else {
                panic!("tuned plan must stay And");
            };
            assert_eq!(grams.len(), 1);
            assert_eq!(grams[0].as_bytes(), b"zqx");
        }

        #[test]
        fn tune_keeps_the_rarest_stop_gram_when_all_are_stops() {
            let df = df_of(&[(b"the", 990), (b"ing", 900)], 1000);
            let mut all_stop = QueryPlan::And {
                grams: vec![Gram::from(&b"the"[..]), Gram::from(&b"ing"[..])],
                sub: vec![],
            };
            all_stop.tune(&df, 500);
            let QueryPlan::And { grams, .. } = &all_stop else {
                panic!("tuned plan must stay And");
            };
            assert_eq!(
                grams.len(),
                1,
                "the rarest stop gram survives as the last discriminator"
            );
            assert_eq!(grams[0].as_bytes(), b"ing");
        }

        #[test]
        fn tune_never_thins_or_bags() {
            let df = df_of(&[(b"the", 990), (b"zqx", 2)], 1000);
            let mut plan = QueryPlan::Or {
                grams: vec![Gram::from(&b"the"[..]), Gram::from(&b"zqx"[..])],
                sub: vec![],
            };
            plan.tune(&df, 500);
            let QueryPlan::Or { grams, .. } = &plan else {
                panic!("tuned plan must stay Or");
            };
            assert_eq!(grams.len(), 2, "every Or branch must survive tuning");
        }
    }

    #[test]
    fn shapes_match_codesearch_regexp_test() {
        // A conjunctive gram bag is `G`; a disjunctive bag is `O`. Cases and
        // expected operator shapes are codesearch's, except where sparse-native
        // branch flushing intentionally keeps more constraints.
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
        assert_shape("(?i)abc", "O");
        assert_shape("(?i)ab~", "O");
        assert_shape(r"\babc", "G");
    }
}
