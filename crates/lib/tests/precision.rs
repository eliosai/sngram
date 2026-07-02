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

use sngram::{Pattern, QueryPlan, query, scan};
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

/// Assert the plan rejects a document the regex does not match. The document
/// must genuinely not match (checked against an oracle) so the plan is free
/// to reject it; rejecting it proves the plan retains enough structure.
fn assert_rejects(re: &str, doc: &[u8]) {
    let t = weight_table();
    let oracle = regex_lite::Regex::new(re).expect("oracle parses pattern");
    let text = String::from_utf8_lossy(doc);
    assert!(
        !oracle.is_match(&text),
        "test bug: {re:?} actually matches {text:?}; pick a non-matching doc"
    );
    let plan = query(&t, &Pattern::new(re).expect("pattern parses"));
    assert!(
        !satisfies(&plan, &index_grams(&t, doc)),
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
    let rendered = plan_of("a{3,}bcd").to_string();
    assert!(
        rendered.contains("aaa"),
        "expected expanded minimum copies in {rendered}"
    );
}

#[test]
fn counted_group_repetition_is_not_all() {
    assert!(!matches!(plan_of("(abc){2}"), QueryPlan::All));
    assert_eq!(plan_of("(abc){2}"), plan_of("abcabc"));
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
fn gap_islands_are_both_required() {
    assert_rejects("sched.*clock", b"schedule the meeting");
    assert_rejects("sched.*clock", b"clock without the other word");
}

#[test]
fn unicode_case_fold_stays_constrained() {
    assert!(!matches!(plan_of("(?i)björn_qux"), QueryPlan::All));
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
