#![allow(
    missing_docs,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
)]

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use sngram::Pattern;
use sngram_types::WeightTable;

fn crc32_table() -> WeightTable {
    let mut buf = vec![0u8; 262_160];
    buf[..4].copy_from_slice(b"SPNG");
    buf[4..8].copy_from_slice(&1u32.to_le_bytes());

    let data = &mut buf[16..];
    for c1 in 0u16..256 {
        for c2 in 0u16..256 {
            let pair = [c1 as u8, c2 as u8];
            let w = crc32fast::hash(&pair);
            let idx = (c1 as usize) << 8 | c2 as usize;
            data[idx * 4..idx * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
    }

    let crc = crc32fast::hash(&buf[16..]);
    buf[8..12].copy_from_slice(&crc.to_le_bytes());
    WeightTable::from_bytes(&buf).unwrap()
}

const PATTERNS: &[(&str, &str)] = &[
    ("literal_short", "MAX_FILE"),
    ("literal_long", "MAX_FILE_SIZE_LIMIT_EXCEEDED"),
    ("wildcard_mid", r"MAX_[A-Z]+_SIZE"),
    ("alternation", r"(foo|bar|baz)_handler"),
    ("prefix_suffix", r"/usr/local/.*\.conf"),
    ("case_insensitive", r"(?i)error"),
    ("complex", r"fn\s+\w+\(.*\)\s*->"),
];

fn bench_pattern_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("pattern/parse");

    for &(name, pat) in PATTERNS {
        group.bench_with_input(BenchmarkId::new("parse", name), &pat, |b, pat| {
            b.iter(|| Pattern::new(pat));
        });
    }
    group.finish();
}

fn bench_query(c: &mut Criterion) {
    let table = crc32_table();
    let mut group = c.benchmark_group("query/extract");

    for &(name, pat) in PATTERNS {
        let pattern = match Pattern::new(pat) {
            Ok(p) => p,
            Err(_) => continue,
        };
        group.bench_with_input(BenchmarkId::new("query", name), &pattern, |b, p| {
            b.iter(|| sngram::query(&table, p));
        });
    }
    group.finish();
}

fn bench_table_load(c: &mut Criterion) {
    let mut buf = vec![0u8; 262_160];
    buf[..4].copy_from_slice(b"SPNG");
    buf[4..8].copy_from_slice(&1u32.to_le_bytes());
    for i in 0..65_536u32 {
        let pair = [(i >> 8) as u8, i as u8];
        let w = crc32fast::hash(&pair);
        buf[16 + i as usize * 4..16 + i as usize * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    let crc = crc32fast::hash(&buf[16..]);
    buf[8..12].copy_from_slice(&crc.to_le_bytes());

    c.bench_function("table/from_bytes", |b| {
        b.iter(|| WeightTable::from_bytes(&buf));
    });
}

criterion_group!(benches, bench_pattern_parse, bench_query, bench_table_load);
criterion_main!(benches);
