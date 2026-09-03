#![allow(
    missing_docs,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::cast_possible_truncation,
    clippy::unwrap_used,
    clippy::expect_used
)]

use std::fmt::{self, Display};

use divan::Bencher;
use sngram::WeightTable;

fn main() {
    divan::main();
}

fn weight_table() -> WeightTable {
    WeightTable::from_weight_fn(|first, second| crc32fast::hash(&[first, second]))
}

#[derive(Clone, Copy)]
struct Pattern {
    name: &'static str,
    regex: &'static str,
}

impl Display for Pattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name)
    }
}

const PATTERNS: [Pattern; 27] = [
    Pattern::new("literal_short", "MAX_FILE"),
    Pattern::new("literal_long", "MAX_FILE_SIZE_LIMIT_EXCEEDED"),
    Pattern::new("wildcard_mid", r"MAX_[A-Z]+_SIZE"),
    Pattern::new("alternation", r"(foo|bar|baz)_handler"),
    Pattern::new("prefix_suffix", r"/usr/local/.*\.conf"),
    Pattern::new("case_insensitive", r"(?i)error"),
    Pattern::new("complex", r"fn\s+\w+\(.*\)\s*->"),
    Pattern::new("todo_fixme", r"TODO|FIXME"),
    Pattern::new("derive_attr", r"#\[derive\("),
    Pattern::new("unwrap_call", r"\.unwrap\(\)"),
    Pattern::new("pub_async_fn", "pub async fn"),
    Pattern::new("trait_impl", r"impl .* for "),
    Pattern::new("error_return", r"return Err\("),
    Pattern::new("use_import", "use crate::"),
    Pattern::new("fn_def", r"fn \w+\("),
    Pattern::new("struct_field_vis", r"pub(\(crate\))? \w+:"),
    Pattern::new("sql_containment", "grams @> ARRAY"),
    Pattern::new("nested_rep_deep", r"((((((abc|abd){4}){4}){4}){4}){4}){4}"),
    Pattern::new(
        "ci_long_run",
        r"(?i)abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz",
    ),
    Pattern::new(
        "alt_wildcard_wide",
        r"(aa000.*bb000|aa001.*bb001|aa002.*bb002|aa003.*bb003|aa004.*bb004|aa005.*bb005)",
    ),
    Pattern::new(
        "hex_uuid",
        r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
    ),
    Pattern::new("anchor_indent_define", r"^[ \t]*#define CONFIG"),
    Pattern::new("anchor_trailing_ws", r"EXPORT_SYMBOL\(\w+\);[ \t]*$"),
    Pattern::new("wide_mixed_unicode_left", r"[A-Za-z\p{Greek}]term_var"),
    Pattern::new("wide_mixed_branch_mix", r"read[A-Za-z\p{Cyrillic}]lock"),
    Pattern::new("unicode_word_boundary", r"\bµs\b"),
    Pattern::new(
        "uuid_hex",
        r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}",
    ),
];

impl Pattern {
    const fn new(name: &'static str, regex: &'static str) -> Self {
        Self { name, regex }
    }
}

#[divan::bench(args = PATTERNS)]
fn query(bencher: Bencher, pattern: Pattern) {
    let table = weight_table();
    bencher.bench(|| sngram::query(&table, divan::black_box(pattern.regex)));
}

#[divan::bench(name = "ci_max_run")]
fn query_ci_max_run(bencher: Bencher) {
    let table = weight_table();
    let pattern = format!("(?i){}", "a".repeat(4000));
    bencher.bench(|| sngram::query(&table, divan::black_box(&pattern)));
}

#[divan::bench(name = "ci_max_varied")]
fn query_ci_max_varied(bencher: Bencher) {
    let table = weight_table();
    let pattern = format!("(?i){}", "abcdefghijklmnop".repeat(250));
    bencher.bench(|| sngram::query(&table, divan::black_box(&pattern)));
}

#[divan::bench]
fn table_from_bytes(bencher: Bencher) {
    let bytes = weight_table().to_bytes();
    bencher.bench(|| WeightTable::from_bytes(divan::black_box(&bytes)));
}
