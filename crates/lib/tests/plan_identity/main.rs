//! Plan identity: every pattern must still plan to exactly the plan it did.
//!
//! [`crate::query`] is a prefilter, so a plan that loses one gram silently
//! loses matches. `plans.tsv` pins the rendered plan of a frozen corpus:
//! every pattern in the query benchmark, the `eg` false-positive query
//! corpus, degenerate and empty patterns, and generated patterns covering
//! alternation, nested repetition, case-insensitivity, unicode classes,
//! anchors, and bounded and unbounded repeats.
//!
//! Regenerate the file only when a planner change is meant to change plans,
//! and prove the new plans are supersets of the old ones before doing so.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use sngram::WeightTable;

const PLANS: &str = include_str!("plans.tsv");

fn table() -> WeightTable {
    WeightTable::from_weight_fn(|c1, c2| crc32fast::hash(&[c1, c2]))
}

/// FNV-1a 64, spelled out so the pinned digests never depend on a library.
fn digest(text: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

fn render(table: &WeightTable, pattern: &str) -> String {
    match sngram::query(table, pattern) {
        Ok(plan) => format!("{plan}"),
        Err(err) => format!("ERR {err}"),
    }
}

struct Pinned<'a> {
    digest: u64,
    len: usize,
    pattern: &'a str,
}

fn pinned() -> Vec<Pinned<'static>> {
    PLANS
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| {
            let mut cols = line.splitn(3, '\t');
            let digest = cols.next().expect("digest column");
            let len = cols.next().expect("length column");
            Pinned {
                digest: u64::from_str_radix(digest, 16).expect("hex digest"),
                len: len.parse().expect("decimal length"),
                pattern: cols.next().expect("pattern column"),
            }
        })
        .collect()
}

#[test]
fn every_pinned_pattern_plans_identically() {
    let table = table();
    let cases = pinned();
    assert!(cases.len() > 600, "corpus shrank to {}", cases.len());
    for case in &cases {
        let plan = render(&table, case.pattern);
        assert_eq!(
            (digest(&plan), plan.len()),
            (case.digest, case.len),
            "plan changed for {:?}",
            case.pattern
        );
    }
}

#[test]
fn the_corpus_covers_the_shapes_that_stress_the_planner() {
    let patterns: Vec<&str> = pinned().into_iter().map(|case| case.pattern).collect();
    let has = |needle: &str| patterns.iter().any(|p| p.contains(needle));
    assert!(patterns.iter().any(|p| p.is_empty()), "empty pattern");
    assert!(patterns.iter().any(|p| p.len() > 3000), "saturating run");
    assert!(has("(?i)"), "case-insensitive");
    assert!(has("\\p{"), "unicode class");
    assert!(has("\\b"), "word boundary");
    assert!(has("$"), "anchor");
    assert!(has("{4}"), "bounded repeat");
    assert!(has("+"), "unbounded repeat");
    assert!(has("|"), "alternation");
}

#[test]
fn the_digest_separates_plans_that_differ() {
    let table = table();
    let one = render(&table, "MAX_FILE");
    let other = render(&table, "MAX_FILT");
    assert_ne!(one, other);
    assert_ne!(digest(&one), digest(&other));
}
