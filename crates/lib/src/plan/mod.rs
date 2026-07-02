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

use core::fmt;

use regex_syntax::hir::Hir;
use sngram_types::WeightTable;

use crate::error::QueryError;
use crate::gram::Gram;
use crate::pattern::{self, Pattern};
use analyze::Analyzer;
use query::{Op, Query};

pub use options::{PlanCase, PlanOptions, PlanSyntax};

/// Nest limit matching `grep-regex`'s translator, so any pattern the
/// verifier accepts also parses here.
const VERIFIER_NEST_LIMIT: u32 = 250;

/// A conservative boolean query over sparse-gram presence.
///
/// Every document the source regex matches satisfies this plan. The plan also
/// admits non-matches, which the exact regex removes afterward.
///
/// The structure mirrors Google codesearch's `Query`: each [`Self::And`] and
/// [`Self::Or`] node carries a bag of grams alongside its sub-plans, so the
/// grams translate to a single array operation. With a postgres `int4[]`
/// column of gram hashes, an `And` bag is `grams @> ARRAY[..]` (all present)
/// and an `Or` bag is `grams && ARRAY[..]` (any present). Each [`Gram`]'s
/// 64-bit key comes from [`Gram::hash`], matching the keys [`crate::scan`]
/// emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryPlan {
    /// No constraint: the index cannot narrow this query.
    All,
    /// Provably empty: no document can match.
    None,
    /// All of `grams` are present and every sub-plan holds.
    And {
        /// Grams that must all be present.
        grams: Vec<Gram>,
        /// Sub-plans that must all hold.
        sub: Vec<Self>,
    },
    /// At least one of `grams` is present, or some sub-plan holds.
    Or {
        /// Grams of which at least one must be present.
        grams: Vec<Gram>,
        /// Sub-plans of which at least one must hold.
        sub: Vec<Self>,
    },
}

/// Decompose a regex pattern into a sparse-gram query plan.
///
/// Infallible, like codesearch's `RegexpQuery`: a pattern with no usable grams
/// yields [`QueryPlan::All`] (the caller decides whether to scan or reject),
/// and an impossible pattern yields [`QueryPlan::None`].
pub(crate) fn query(table: &WeightTable, pattern: &Pattern) -> QueryPlan {
    query_hir(table, pattern.hir())
}

/// Decompose one or more patterns under the verifier's match options.
///
/// The patterns are escaped (for fixed strings), OR-joined, and parsed with
/// the same flags the verifying engine uses — including engine-rule smart
/// case — so the plan can never assume narrower semantics than the match
/// run that follows. Inversion yields [`QueryPlan::All`]: every document may
/// hold a non-matching line.
pub(crate) fn query_with<P: AsRef<str>>(
    table: &WeightTable,
    patterns: &[P],
    opts: PlanOptions,
) -> Result<QueryPlan, QueryError> {
    if opts.invert || patterns.is_empty() {
        return Ok(QueryPlan::All);
    }
    let mut parts = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        let pattern = pattern.as_ref();
        match opts.syntax {
            PlanSyntax::Regex => parts.push(format!("(?:{pattern})")),
            PlanSyntax::FixedStrings => {
                parts.push(format!("(?:{})", regex_syntax::escape(pattern)));
            },
        }
    }
    let joined = parts.join("|");
    pattern::check_length(&joined)?;
    let hir = parse_joined(&joined, opts)?;
    Ok(query_hir(table, &hir))
}

/// Parse the joined pattern with the flags the verifying engine uses.
fn parse_joined(joined: &str, opts: PlanOptions) -> Result<Hir, QueryError> {
    let insensitive = match opts.case {
        PlanCase::Sensitive => false,
        PlanCase::Insensitive => true,
        PlanCase::Smart => options::smart_case_insensitive(joined),
    };
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

/// Fold an already-parsed HIR into a plan.
fn query_hir(table: &WeightTable, hir: &Hir) -> QueryPlan {
    let analyzer = Analyzer::new(table);
    let mut info = analyzer.analyze(hir);
    analyzer.begin_final_flush();
    analyzer.simplify(&mut info, true);
    analyzer.add_exact(&mut info);
    to_plan(info.match_)
}

/// Renders like codesearch's `Query.String()`: `+` for all, `-` for none, a
/// quoted gram for a lone leaf, space-joined for `And`, `(..)|(..)` for `Or`.
impl fmt::Display for QueryPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::All => f.write_str("+"),
            Self::None => f.write_str("-"),
            Self::And { grams, sub } => write_join(f, grams, sub, " ", false),
            Self::Or { grams, sub } => write_join(f, grams, sub, "|", true),
        }
    }
}

/// Write the grams then sub-plans of an `And`/`Or` joined by `sep`; when
/// `paren`, wrap each multi-token operand in parentheses.
fn write_join(
    f: &mut fmt::Formatter<'_>,
    grams: &[Gram],
    sub: &[QueryPlan],
    sep: &str,
    paren: bool,
) -> fmt::Result {
    let mut first = true;
    for gram in grams {
        delimit(f, &mut first, sep)?;
        write!(f, "{:?}", String::from_utf8_lossy(gram.as_bytes()))?;
    }
    for plan in sub {
        delimit(f, &mut first, sep)?;
        if paren {
            write!(f, "({plan})")?;
        } else {
            write!(f, "{plan}")?;
        }
    }
    Ok(())
}

fn delimit(f: &mut fmt::Formatter<'_>, first: &mut bool, sep: &str) -> fmt::Result {
    if *first {
        *first = false;
        Ok(())
    } else {
        f.write_str(sep)
    }
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

    use sngram_types::{TABLE_BINARY_SIZE, WeightTable};

    use super::QueryPlan;
    use crate::pattern::Pattern;
    use crate::query;

    /// A deterministic weight table: each byte pair hashed to a varied weight,
    /// so the sparse hull is non-trivial.
    fn table() -> WeightTable {
        let mut buf = vec![0u8; TABLE_BINARY_SIZE];
        buf[..4].copy_from_slice(b"SPNG");
        buf[4..8].copy_from_slice(&1u32.to_le_bytes());
        for c1 in 0u8..=255 {
            for c2 in 0u8..=255 {
                let w = crc32fast::hash(&[c1, c2]);
                let idx = (usize::from(c1) << 8) | usize::from(c2);
                let off = 16 + idx * 4;
                buf[off..off + 4].copy_from_slice(&w.to_le_bytes());
            }
        }
        let crc = crc32fast::hash(&buf[16..]);
        buf[8..12].copy_from_slice(&crc.to_le_bytes());
        WeightTable::from_bytes(&buf).unwrap()
    }

    fn plan_of(re: &str) -> QueryPlan {
        query(&table(), &Pattern::new(re).unwrap())
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
        //! `query_with` must build plans from exactly the verifier's
        //! semantics: engine-rule smart case, fixed-string escaping,
        //! OR-joined multiple patterns, and All for inversion.

        use super::{plan_of, table};
        use crate::plan::{PlanCase, PlanOptions, PlanSyntax, QueryPlan, query_with};

        fn with(opts: PlanOptions, patterns: &[&str]) -> QueryPlan {
            query_with(&table(), patterns, opts).expect("patterns plan")
        }

        #[test]
        fn smart_case_folds_when_uppercase_only_in_escapes() {
            // grep-regex treats \W as class, not literal: the verifier
            // matches insensitively, so the plan must too.
            let smart = with(
                PlanOptions {
                    case: PlanCase::Smart,
                    ..PlanOptions::default()
                },
                &[r"maxfile\Wsize"],
            );
            let insensitive = with(
                PlanOptions {
                    case: PlanCase::Insensitive,
                    ..PlanOptions::default()
                },
                &[r"maxfile\Wsize"],
            );
            assert_eq!(smart, insensitive);
        }

        #[test]
        fn smart_case_stays_sensitive_on_uppercase_literals() {
            let smart = with(
                PlanOptions {
                    case: PlanCase::Smart,
                    ..PlanOptions::default()
                },
                &["MaxFile"],
            );
            let sensitive = with(PlanOptions::default(), &["MaxFile"]);
            assert_eq!(smart, sensitive);
        }

        #[test]
        fn fixed_strings_escape_metacharacters() {
            let fixed = with(
                PlanOptions {
                    syntax: PlanSyntax::FixedStrings,
                    ..PlanOptions::default()
                },
                &["max.*size"],
            );
            assert_eq!(fixed, plan_of(r"max\.\*size"));
        }

        #[test]
        fn invert_plans_everything() {
            let inverted = with(
                PlanOptions {
                    invert: true,
                    ..PlanOptions::default()
                },
                &["max_file_size"],
            );
            assert_eq!(inverted, QueryPlan::All);
        }

        #[test]
        fn multiple_patterns_join_as_alternation() {
            let joined = with(PlanOptions::default(), &["max_file", "min_file"]);
            assert_eq!(joined, plan_of("(?:max_file)|(?:min_file)"));
        }

        #[test]
        fn insensitive_flag_equals_inline_wrapping() {
            // eg used to wrap patterns as (?i:..) before parsing; the flag
            // path must plan identically.
            for pat in ["max_file_size", "sched[_-]clock", "mem+set"] {
                let flagged = with(
                    PlanOptions {
                        case: PlanCase::Insensitive,
                        ..PlanOptions::default()
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
            let default = with(PlanOptions::default(), &[r"foo$\r\nbar"]);
            assert_eq!(default, QueryPlan::None);
            let crlf = with(
                PlanOptions {
                    crlf: true,
                    ..PlanOptions::default()
                },
                &[r"foo$\r\nbar"],
            );
            assert!(!matches!(crlf, QueryPlan::None));
        }

        #[test]
        fn non_unicode_byte_patterns_parse() {
            let opts = PlanOptions {
                unicode: false,
                ..PlanOptions::default()
            };
            let plan = query_with(&table(), &[r"foobar\xFF"], opts).expect("bytes plan");
            assert!(!matches!(plan, QueryPlan::All));
        }
    }

    #[test]
    fn shapes_match_codesearch_regexp_test() {
        // A conjunctive gram bag is `G`; a disjunctive bag is `O`. Cases and
        // expected operator shapes are codesearch's.
        assert_shape("Abcdef", "G");
        assert_shape("(abc)(def)", "G");
        assert_shape("abc.*(def|ghi)", "(G & O)");
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
