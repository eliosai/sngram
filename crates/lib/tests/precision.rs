//! Plan-strength regressions: documents a stronger plan must reject.
//!
//! Soundness (`soundness.rs`) guarantees no false negatives; this file pins
//! the other direction. Each case is a crafted near-miss document that
//! contains enough gram fragments to fool a weak plan but does not match the
//! regex. A failure here is a precision regression: the plan admitted a
//! document it has enough information to reject. Unlike soundness these are
//! not invariants of correctness, they are the quality bar for the planner.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "tests assert by panicking; the fixture indexes a fixed-shape buffer"
)]

use std::collections::HashSet;

use sngram::{
    IndexFormat, Pattern, PlanOptions, QueryPlan, ScanOptions, plan_query, query, scan, scan_with,
};
use sngram_types::{Content, TABLE_BINARY_SIZE, WeightTable};

/// A deterministic weight table: each byte pair hashed to a varied weight, so
/// the sparse hull is non-trivial.
fn weight_table() -> WeightTable {
    let mut buf = vec![0u8; TABLE_BINARY_SIZE];
    buf[..4].copy_from_slice(b"SPNG");
    buf[4..8].copy_from_slice(&1u32.to_le_bytes());
    for c1 in 0u8..=255 {
        for c2 in 0u8..=255 {
            let w = crc32fast::hash(&[c1, c2]);
            let idx = (usize::from(c1) << 8) | usize::from(c2);
            buf[16 + idx * 4..16 + idx * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
    }
    let crc = crc32fast::hash(&buf[16..]);
    buf[8..12].copy_from_slice(&crc.to_le_bytes());
    WeightTable::from_bytes(&buf).unwrap()
}

/// Evaluate a plan against the grams a document indexed to.
fn satisfies(plan: &QueryPlan, grams: &HashSet<Vec<u8>>) -> bool {
    match plan {
        QueryPlan::All => true,
        QueryPlan::None => false,
        QueryPlan::And { grams: g, sub } => {
            g.iter().all(|x| grams.contains(x.as_bytes()))
                && sub.iter().all(|s| satisfies(s, grams))
        },
        QueryPlan::Or { grams: g, sub } => {
            g.iter().any(|x| grams.contains(x.as_bytes()))
                || sub.iter().any(|s| satisfies(s, grams))
        },
    }
}

fn index_grams(t: &WeightTable, doc: &[u8]) -> HashSet<Vec<u8>> {
    let mut grams = HashSet::new();
    scan(t, &Content::new(doc), |s, e, _| {
        grams.insert(doc[s..e].to_vec());
    });
    grams
}

fn index_grams_with(t: &WeightTable, doc: &[u8], opts: ScanOptions) -> HashSet<Vec<u8>> {
    let mut grams = HashSet::new();
    scan_with(t, &Content::new(doc), opts, |gram, _| {
        grams.insert(gram.to_vec());
    });
    grams
}

/// Assert the plan rejects a document the regex does not match. The document
/// must genuinely not match (checked against an oracle) so the plan is free
/// to reject it; rejecting it proves the plan retains enough structure.
fn assert_rejects(re: &str, doc: &[u8]) {
    let t = weight_table();
    // The full `regex` crate over raw bytes, matching the soundness oracle and
    // the engine eg verifies with: it supports `\p{..}` unicode classes and
    // reads the document as the bytes the index scanned.
    let oracle = regex::bytes::Regex::new(re).expect("oracle parses pattern");
    let text = String::from_utf8_lossy(doc);
    assert!(
        !oracle.is_match(doc),
        "test bug: {re:?} actually matches {text:?}; pick a non-matching doc"
    );
    let plan = query(&t, &Pattern::new(re).expect("pattern parses"));
    assert!(
        !satisfies(&plan, &index_grams(&t, doc)),
        "PRECISION REGRESSION: {re:?} plan {plan} admits non-matching {text:?}"
    );
}

fn assert_planned_rejects(
    re: &str,
    doc: &[u8],
    opts: PlanOptions,
    format: IndexFormat,
    scan_opts: ScanOptions,
) {
    let t = weight_table();
    let oracle = regex::bytes::Regex::new(re).expect("oracle parses pattern");
    let text = String::from_utf8_lossy(doc);
    assert!(
        !oracle.is_match(doc),
        "test bug: {re:?} actually matches {text:?}; pick a non-matching doc"
    );
    let plan = plan_query(&t, &[re], opts, format)
        .expect("pattern plans")
        .plan;
    assert!(
        !satisfies(&plan, &index_grams_with(&t, doc, scan_opts)),
        "PRECISION REGRESSION: {re:?} plan {plan} admits non-matching {text:?}"
    );
}

/// The longest gram anywhere in the plan, in bytes.
fn max_gram_len(plan: &QueryPlan) -> usize {
    match plan {
        QueryPlan::All | QueryPlan::None => 0,
        QueryPlan::And { grams, sub } | QueryPlan::Or { grams, sub } => grams
            .iter()
            .map(|g| g.len())
            .chain(sub.iter().map(max_gram_len))
            .max()
            .unwrap_or(0),
    }
}

fn plan_of(re: &str) -> QueryPlan {
    query(&weight_table(), &Pattern::new(re).expect("pattern parses"))
}

// --- case-insensitive queries must keep wide windows, not 3-byte chunks ---

#[test]
fn case_insensitive_plan_keeps_wide_windows() {
    let plan = plan_of("(?i)max_file_size");
    assert!(
        max_gram_len(&plan) >= 6,
        "expected a wide case-folded window, got only short grams: {plan}"
    );
}

#[test]
fn case_insensitive_plan_keeps_wide_windows_past_first_flush() {
    // Long enough that the analyzer must flush several windows; the windows
    // after the first flush must stay wide too. The tail here is
    // "initcall"; a trigram-era planner leaves only 3-byte grams covering it.
    let plan = plan_of("(?i)subsys_initcall");
    let tail = tail_gram_max_len(&plan);
    assert!(
        tail >= 6,
        "expected wide windows after the first flush, got {tail}-byte tail grams: {plan}"
    );
}

/// Longest gram that lies entirely within the case-folded tail of the
/// pattern (chars from `initcall`), proving late windows stay wide.
fn tail_gram_max_len(plan: &QueryPlan) -> usize {
    fn in_tail(g: &[u8]) -> bool {
        let lower: Vec<u8> = g.iter().map(u8::to_ascii_lowercase).collect();
        b"initcall"
            .windows(lower.len())
            .any(|w| w == lower.as_slice())
    }
    fn walk(plan: &QueryPlan, best: &mut usize) {
        if let QueryPlan::And { grams, sub } | QueryPlan::Or { grams, sub } = plan {
            for g in grams.iter().filter(|g| in_tail(g)) {
                *best = (*best).max(g.len());
            }
            for s in sub {
                walk(s, best);
            }
        }
    }
    let mut best = 0;
    walk(plan, &mut best);
    best
}

#[test]
fn case_insensitive_rejects_scattered_fragments() {
    // Contains every case-variant trigram window of "max_file_size"
    // ("max", "ax_", "x_f", "_fi", "fil", "ile", "le_", "e_s", "_si",
    // "siz", "ize") scattered across words, but never 6 consecutive
    // characters of any case variant of the pattern.
    assert_rejects(
        "(?i)max_file_size",
        b"max_ ax_f x_fi _fil file ile_ le_si e_si _siz size prize",
    );
}

// --- bounded repetition must expand, not degenerate to one copy ---

#[test]
fn exact_repetition_matches_expanded_plan() {
    assert_eq!(plan_of("(ab){3}"), plan_of("ababab"));
}

#[test]
fn min_repetition_keeps_expanded_prefix() {
    // The expanded copies must bind to the following literal: a document
    // holding "abcd" but never a double-a run before it must be rejected.
    assert_rejects("a{3,}bcd", b"aaaa then abcd");
}

#[test]
fn counted_group_repetition_is_not_all() {
    assert!(!matches!(plan_of("(abc){2}"), QueryPlan::All));
    assert_eq!(plan_of("(abc){2}"), plan_of("abcabc"));
}

#[test]
fn bounded_range_above_cap_rejects_short_run() {
    // `h{3,5}` has max 5 > MAX_REPEAT_EXPAND, but the base is a one-byte exact
    // set and the range is narrow, so it enumerates to {hhh,hhhh,hhhhh}·i
    // instead of collapsing to a single demoted copy. A demoted plan keeps
    // only "hhi" and admits any two-h run before an "i"; the enumerated plan
    // requires "hhh" too. "ahhi" has just two h's, so it must be rejected.
    assert_rejects("h{3,5}i", b"ahhi");
}

// --- alternation branches must not cross-mix prefixes and suffixes ---

#[test]
fn alternation_rejects_mixed_branch_fragments() {
    // "hello"+"fghij" mixes branch 1's prefix with branch 2's suffix.
    assert_rejects("hello+xyzzy|abcde+fghij", b"hello wxyz fghij abcd exyzzy");
}

// --- regressions that already hold and must keep holding ---

#[test]
fn small_class_rejects_unbridged_variant() {
    assert_rejects("sched[_-]clock", b"sched clock scheduler clocksource");
}

#[test]
fn optional_prefix_keeps_required_suffix() {
    assert_rejects("(?:un)?likely\\(", b"likely unlikely liked");
}

#[test]
fn plus_seam_is_covered() {
    assert_rejects("mem+set", b"memory setup offset");
}

#[test]
fn plus_seam_rejects_reordered_run() {
    // `mem+set` is `me·m+·set`. Every match is "memset" (one m) or contains
    // "mmset" (two or more), so the sound suffix after crossing the run with
    // "set" is {memset, mmset}. A plan that only knows the single-byte run
    // base admits "mset" anywhere after "mem"; "memory mset here" holds "mem",
    // "mse", and "set" as scattered fragments but neither "memset" nor
    // "mmset", so the seam-aware plan rejects it.
    assert_rejects("mem+set", b"memory mset here");
}

#[test]
fn plus_seam_rejects_split_literal() {
    // `rea+d_lock` is `re·a+·d_lock`; the sound suffix after the seam is
    // {read_lock, aad_lock}. "realized ad_lock" carries "rea" and every gram
    // of "ad_lock" but neither full seam string (no "read" run, no "aad"), so
    // it is rejected — a plan blind to the seam admits it on the fragments.
    assert_rejects("rea+d_lock", b"realized ad_lock");
}

#[test]
fn wide_class_pair_keeps_the_trailing_seam() {
    // Two enumerated classes cross to 52x52 strings; truncating the
    // boundary set all the way to empty strings would sever the seam to
    // "lock" and admit any document with "read" and "lock" fragments.
    assert_rejects("read[a-zA-Z][a-zA-Z]lock", b"readb lock held");
}

#[test]
fn finite_class_pair_keeps_correlated_middle_window() {
    // The file contains a valid left seam ("readb") and a valid right seam
    // ("block"), but never the same two class bytes between "read" and
    // "lock". A plan that flushes the two sides independently admits it.
    assert_rejects("read[a-zA-Z][a-zA-Z]lock", b"readb fast block held");
}

#[test]
fn numeric_plus_keeps_literal_prefix_context() {
    // A semver-like regex with a literal prefix must not degenerate into
    // only the broad digit-dot-digit middle window.
    assert_rejects(r"v[0-9]+\.[0-9]+\.[0-9]+", b"release 1.2.3 without v");
}

// --- wide classes keep a boundary-byte seam to their neighbours ---

#[test]
fn wide_class_before_literal_keeps_boundary_seam() {
    // \p{Greek} exceeds the enumeration cap, but every Greek scalar's UTF-8
    // encoding ends in a continuation byte, so the plan requires some
    // <continuation-byte>term_var window. This document has "term_var" but
    // preceded by an ASCII space, so no such window exists and it is rejected
    // — where a bare any_char (prefix/suffix {""}) would admit it.
    assert_rejects(r"\p{Greek}term_var", b"the term_var here");
}

#[test]
fn wide_class_after_literal_keeps_boundary_seam() {
    // Symmetric to the leading case: term_var\p{Greek} requires a
    // term_var<Greek-lead-byte> window, which "term_var" followed by an
    // ASCII space (or end of line) lacks.
    assert_rejects(r"term_var\p{Greek}", b"a term_var stops here");
}

#[test]
fn wide_class_between_literals_keeps_both_seams() {
    // A wide class flanked by literals crosses its lead bytes into the left
    // literal and its continuation bytes into the right one, so both edges
    // stay anchored.
    assert_rejects(r"read\p{Cyrillic}lock", b"read lock, readlock, red lock");
}

#[test]
fn mixed_wide_class_before_literal_keeps_boundary_seam() {
    assert_rejects(r"[A-Za-z\p{Greek}]term_var", b"the term_var here");
}

#[test]
fn mixed_wide_class_between_literals_keeps_boundary_seams() {
    assert_rejects(r"read[A-Za-z\p{Cyrillic}]lock", b"read lock");
}

#[test]
fn mixed_wide_class_between_literals_rejects_missing_class_byte() {
    assert_rejects(r"read[A-Za-z\p{Cyrillic}]lock", b"readlock");
}

#[test]
fn open_repetition_keeps_multi_copy_seam() {
    // x{n,} expands as x+ then full copies, so the copies sit adjacent to
    // the following literal: a one-copy suffix window ("abcd") admits
    // documents lacking the repeated run entirely.
    assert_rejects("a{5,}bcd", b"aaaaaaa then abcd");
}

#[test]
fn budget_reaches_the_pattern_tail() {
    // A long case-folded pattern must keep some constraint on its tail;
    // spending the whole gram budget on the head admits any document
    // holding only the head.
    assert_rejects(
        "(?i)trace_event_raw_event_sched_switch",
        b"trace_event_raw_event noth",
    );
}

#[test]
fn budget_reaches_a_sixty_char_tail() {
    // Long enough that the budget gate closes mid-pattern: the final flush
    // of the pattern's own edges must still land, or a document holding
    // only the first two thirds is admitted.
    assert_rejects(
        "(?i)very_long_function_name_with_many_parts_and_then_some_more_tail",
        b"very_long_function_name_with_many_parts_",
    );
}

#[test]
fn long_literal_after_wide_class_is_fully_covered() {
    // The class multiplies the exact set; fitting the flush must not
    // shorten a long literal whose real covers already fit the budget.
    // The document holds the literal's first 60 bytes (the truncated
    // window) and its last 8 bytes (the spill stub) but not the middle.
    let lit = "the_quick_brown_fox_jumped_over_the_lazy_dog_while_the_cat_watched_from_the_windowsill_yes";
    let doc = format!("Q{} filler {}", &lit[..60], &lit[lit.len() - 8..]);
    assert_rejects(&format!("[!-~]{lit}"), doc.as_bytes());
}

#[test]
fn min_two_repetition_keeps_both_seams() {
    // x{2,} must carry two copies on BOTH edges: this document holds
    // "initxyxy" and "xydone" but never "xyxydone".
    assert_rejects(
        "init(xy){2,}done",
        b"fooxyxy initxyxy lhsxyxy and abdone xydone qzdone end",
    );
}

#[test]
fn gap_islands_are_both_required() {
    assert_rejects("sched.*clock", b"schedule the meeting");
    assert_rejects("sched.*clock", b"clock without the other word");
}

#[test]
fn sentinel_start_anchor_survives_nullable_indent() {
    assert_planned_rejects(
        r"^[ \t]*#define CONFIG",
        b"int x; #define CONFIG_FOO",
        PlanOptions::default(),
        IndexFormat {
            folded_space: false,
            line_sentinels: true,
        },
        ScanOptions {
            line_sentinels: true,
            ..ScanOptions::default()
        },
    );
}

#[test]
fn sentinel_end_anchor_survives_nullable_trailing_space() {
    assert_planned_rejects(
        r"EXPORT_SYMBOL\(\w+\);[ \t]*$",
        b"EXPORT_SYMBOL(foo); trailing",
        PlanOptions::default(),
        IndexFormat {
            folded_space: false,
            line_sentinels: true,
        },
        ScanOptions {
            line_sentinels: true,
            ..ScanOptions::default()
        },
    );
}

#[test]
fn unicode_case_fold_stays_constrained() {
    assert!(!matches!(plan_of("(?i)björn_qux"), QueryPlan::All));
}

#[test]
fn impossible_look_contexts_plan_to_none() {
    // These match nothing: every candidate would be a false positive, and
    // the byte context around the assertion proves it at plan time.
    assert!(matches!(plan_of(r"t\bhe"), QueryPlan::None));
    assert!(matches!(plan_of(r"foo$bar"), QueryPlan::None));
    assert!(matches!(plan_of(r"abc\Adef"), QueryPlan::None));
    assert!(matches!(plan_of(r"kfree\B\("), QueryPlan::None));
}

#[test]
fn unicode_word_boundary_between_words_plans_to_none() {
    assert!(matches!(plan_of(r"a\bµs"), QueryPlan::None));
    assert!(matches!(plan_of(r"µ\bβ"), QueryPlan::None));
}

#[test]
fn unicode_non_word_boundary_between_word_and_word_plans_to_none() {
    assert!(matches!(plan_of(r"a\B-"), QueryPlan::None));
    assert!(matches!(plan_of(r"-\Bµ"), QueryPlan::None));
}

#[test]
fn looks_filter_adjacent_class_members() {
    // A surviving assertion still rules out class members: after word "o",
    // \b kills the word member "x" of [x+], so only "foo+bar" can match.
    assert_rejects(r"foo\b[x+]bar", b"fooxbar but never the other");
    // ^ after \s forces the whitespace byte to be a newline.
    assert_rejects("(?m)foo\\s^bar", b"foo bar and foo\nbaz");
}

#[test]
fn satisfiable_look_contexts_stay_planned() {
    assert!(!matches!(plan_of(r"\bkfree_skb\b"), QueryPlan::None));
    assert!(!matches!(plan_of(r"^static_call"), QueryPlan::None));
    assert!(!matches!(plan_of(r"module_exit$"), QueryPlan::None));
    assert!(!matches!(plan_of(r"foo\B_bar"), QueryPlan::None));
    assert!(!matches!(plan_of(r"end\.\bstart"), QueryPlan::None));
}

// --- plan size must stay bounded under combinatorial patterns ---

/// Total gram instances anywhere in the plan.
fn plan_gram_count(plan: &QueryPlan) -> usize {
    match plan {
        QueryPlan::All | QueryPlan::None => 0,
        QueryPlan::And { grams, sub } | QueryPlan::Or { grams, sub } => {
            grams.len() + sub.iter().map(plan_gram_count).sum::<usize>()
        },
    }
}

#[test]
fn maximal_covers_respect_the_budget() {
    // Few-branch sets take maximal covers; the flush accounting must use
    // their real size, not an estimate calibrated for minimal covers, or
    // one flush overshoots the whole budget and starves what follows.
    let plan = plan_of(
        "(?i:very_long_function_name_with_many_parts).*[a-h]another_extremely_long_trailing_literal_block_that_should_be_covered_maximally_here_nowz",
    );
    let count = plan_gram_count(&plan);
    assert!(count <= 4608, "plan overshot the budget: {count} grams");
}

#[test]
fn wide_class_repetition_keeps_plan_bounded() {
    // A hex-digit class multiplies the exact set by 16 per position; without
    // a cross-product bound this balloons into thousands of OR branches.
    let plan = plan_of("0x[0-9a-f]{8}");
    assert!(!matches!(plan, QueryPlan::All));
    let count = plan_gram_count(&plan);
    assert!(count <= 4096, "plan ballooned to {count} grams");
}

#[test]
fn case_insensitive_long_pattern_keeps_plan_bounded() {
    let plan = plan_of("(?i)netif_receive_skb_list_internal");
    let count = plan_gram_count(&plan);
    assert!(count <= 16384, "plan ballooned to {count} grams");
}

#[test]
fn nested_bounded_repetition_keeps_plan_bounded() {
    // Each nesting level multiplies the expanded copies; without a cap on
    // replicating the accumulated match query this is 4^depth grams and a
    // 64-character pattern plans for seconds.
    let plan = plan_of("((((((((abc|abd){4}){4}){4}){4}){4}){4}){4}){4}");
    let count = plan_gram_count(&plan);
    assert!(count <= 8192, "plan ballooned to {count} grams");
}
