//! Plan size must stay bounded under combinatorial patterns.
#![allow(missing_docs, clippy::expect_used)]

use sngram::query;
use sngram_types::{QueryPlan, WeightTable};

fn weight_table() -> WeightTable {
    WeightTable::from_weight_fn(|c1, c2| crc32fast::hash(&[c1, c2]))
}

fn plan_of(re: &str) -> QueryPlan {
    query(&weight_table(), re).expect("pattern parses")
}

#[test]
fn maximal_covers_respect_the_budget() {
    // Few-branch sets take maximal covers; the flush accounting must use
    // their real size, not an estimate calibrated for minimal covers, or
    // one flush overshoots the whole budget and starves what follows.
    let plan = plan_of(
        "(?i:very_long_function_name_with_many_parts).*[a-h]another_extremely_long_trailing_literal_block_that_should_be_covered_maximally_here_nowz",
    );
    let count = plan.gram_count();
    assert!(count <= 4608, "plan overshot the budget: {count} grams");
}

#[test]
fn wide_class_repetition_keeps_plan_bounded() {
    // A hex-digit class multiplies the exact set by 16 per position; without
    // a cross-product bound this balloons into thousands of OR branches.
    let plan = plan_of("0x[0-9a-f]{8}");
    assert!(!plan.is_all());
    let count = plan.gram_count();
    assert!(count <= 4096, "plan ballooned to {count} grams");
}

#[test]
fn case_insensitive_long_pattern_keeps_plan_bounded() {
    let plan = plan_of("(?i)netif_receive_skb_list_internal");
    let count = plan.gram_count();
    assert!(count <= 16384, "plan ballooned to {count} grams");
}

#[test]
fn nested_bounded_repetition_keeps_plan_bounded() {
    // Each nesting level multiplies the expanded copies; without a cap on
    // replicating the accumulated match query this is 4^depth grams and a
    // 64-character pattern plans for seconds.
    let plan = plan_of("((((((((abc|abd){4}){4}){4}){4}){4}){4}){4}){4}");
    let count = plan.gram_count();
    assert!(count <= 8192, "plan ballooned to {count} grams");
}
