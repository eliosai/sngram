#![allow(
    missing_docs,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::unwrap_used,
    clippy::expect_used
)]

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use sngram::QueryOptions;
use sngram_types::WeightTable;

fn weight_table() -> WeightTable {
    WeightTable::from_weight_fn(|c1, c2| crc32fast::hash(&[c1, c2]))
}

const PATTERNS: &[(&str, &str)] = &[
    ("literal_short", "MAX_FILE"),
    ("literal_long", "MAX_FILE_SIZE_LIMIT_EXCEEDED"),
    ("wildcard_mid", r"MAX_[A-Z]+_SIZE"),
    ("alternation", r"(foo|bar|baz)_handler"),
    ("prefix_suffix", r"/usr/local/.*\.conf"),
    ("case_insensitive", r"(?i)error"),
    ("complex", r"fn\s+\w+\(.*\)\s*->"),
    // Realistic codebase greps, the kind run against this very repo.
    ("todo_fixme", r"TODO|FIXME"),
    ("derive_attr", r"#\[derive\("),
    ("unwrap_call", r"\.unwrap\(\)"),
    ("pub_async_fn", "pub async fn"),
    ("trait_impl", r"impl .* for "),
    ("error_return", r"return Err\("),
    ("use_import", "use crate::"),
    ("fn_def", r"fn \w+\("),
    ("struct_field_vis", r"pub(\(crate\))? \w+:"),
    ("sql_containment", "grams @> ARRAY"),
    // Planner blowup pins: nested bounded repetition (budget-capped),
    // long case-folded runs (cross caps), wide-OR implication checks, and
    // the wide-class repetition path.
    ("nested_rep_deep", r"((((((abc|abd){4}){4}){4}){4}){4}){4}"),
    (
        "ci_long_run",
        r"(?i)abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz",
    ),
    (
        "alt_wildcard_wide",
        r"(aa000.*bb000|aa001.*bb001|aa002.*bb002|aa003.*bb003|aa004.*bb004|aa005.*bb005)",
    ),
    (
        "hex_uuid",
        r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
    ),
    ("anchor_indent_define", r"^[ \t]*#define CONFIG"),
    ("anchor_trailing_ws", r"EXPORT_SYMBOL\(\w+\);[ \t]*$"),
    ("wide_mixed_unicode_left", r"[A-Za-z\p{Greek}]term_var"),
    ("wide_mixed_branch_mix", r"read[A-Za-z\p{Cyrillic}]lock"),
    ("unicode_word_boundary", r"\bµs\b"),
    (
        "uuid_hex",
        r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}",
    ),
];

fn bench_query(c: &mut Criterion) {
    let table = weight_table();
    let mut group = c.benchmark_group("query/extract");

    for &(name, pat) in PATTERNS {
        group.bench_with_input(BenchmarkId::new("query", name), &pat, |b, pat| {
            b.iter(|| sngram::query(&table, core::slice::from_ref(pat), QueryOptions::default()));
        });
    }

    // Maximum-length case-folded runs: the analyzer must go idle once the
    // plan budget saturates instead of folding the rest of the pattern.
    let long_runs = [
        ("ci_max_run", format!("(?i){}", "a".repeat(4000))),
        (
            "ci_max_varied",
            format!("(?i){}", "abcdefghijklmnop".repeat(250)),
        ),
    ];
    for (name, pat) in &long_runs {
        group.bench_with_input(BenchmarkId::new("query", *name), pat, |b, pat| {
            b.iter(|| sngram::query(&table, &[pat.as_str()], QueryOptions::default()));
        });
    }
    group.finish();
}

fn bench_table_load(c: &mut Criterion) {
    let buf = weight_table().to_bytes();

    c.bench_function("table/from_bytes", |b| {
        b.iter(|| WeightTable::from_bytes(&buf));
    });
}

criterion_group!(benches, bench_query, bench_table_load);
criterion_main!(benches);
