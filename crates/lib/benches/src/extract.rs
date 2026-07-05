#![allow(
    missing_docs,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::excessive_nesting,
    clippy::too_many_lines,
    clippy::unwrap_used
)]

use std::{hint::black_box, io::Cursor};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use sngram_types::ScanEvent;
use sngram_types::WeightTable;

fn crc32_table() -> WeightTable {
    WeightTable::from_weight_fn(|c1, c2| crc32fast::hash(&[c1, c2]))
}

fn source_code(size: usize) -> Vec<u8> {
    let src = b"fn main() { let x = foo_bar(42); println!(\"{x}\"); }\n";
    (0..size).map(|i| src[i % src.len()]).collect()
}

fn prose(size: usize) -> Vec<u8> {
    let txt = b"The quick brown fox jumps over the lazy dog. ";
    (0..size).map(|i| txt[i % txt.len()]).collect()
}

fn count_grams(table: &WeightTable, data: &[u8]) -> u64 {
    let mut count = 0u64;
    sngram::scan(table, Cursor::new(data), |event| {
        count += u64::from(matches!(event, ScanEvent::Gram(_)));
    })
    .expect("scan succeeds");
    count
}

const SIZES: &[usize] = &[64, 256, 1024, 4096, 16384, 65536, 262_144, 1_048_576];
const SMALL: &[usize] = &[256, 4096, 65536];

fn bench_weight_lookup(c: &mut Criterion) {
    let table = crc32_table();
    c.bench_function("weight_lookup", |b| {
        let mut i = 0u8;
        let mut j = 0u8;
        b.iter(|| {
            let w = table.weight(i, j);
            j = j.wrapping_add(1);
            if j == 0 {
                i = i.wrapping_add(1);
            }
            w
        });
    });
}

fn bench_scan_code(c: &mut Criterion) {
    let table = crc32_table();
    let mut group = c.benchmark_group("scan/code");

    for &size in SIZES {
        let data = source_code(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, c| {
            b.iter(|| count_grams(&table, c));
        });
    }
    group.finish();
}

fn bench_scan_prose(c: &mut Criterion) {
    let table = crc32_table();
    let mut group = c.benchmark_group("scan/prose");

    for &size in SMALL {
        let data = prose(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, c| {
            b.iter(|| count_grams(&table, c));
        });
    }
    group.finish();
}

fn bench_scan_uniform(c: &mut Criterion) {
    let table = crc32_table();
    let mut group = c.benchmark_group("scan/uniform");

    for &size in SMALL {
        let data = vec![b'a'; size];
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, c| {
            b.iter(|| count_grams(&table, c));
        });
    }
    group.finish();
}

fn bench_scan_ascending(c: &mut Criterion) {
    let table = crc32_table();
    let mut group = c.benchmark_group("scan/ascending");

    for &size in SMALL {
        let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, c| {
            b.iter(|| count_grams(&table, c));
        });
    }
    group.finish();
}

// The workload every real consumer runs: content in, 64-bit index keys out.
// `rehash` is the pre-0.5 shape (hash each emitted gram's bytes from scratch);
// `fused` consumes the rolling hash the scan already computed.
fn bench_pipeline(c: &mut Criterion) {
    use std::hash::{Hash, Hasher};

    let table = crc32_table();
    let mut group = c.benchmark_group("pipeline/code");

    for &size in &[65_536usize, 1_048_576] {
        let data = source_code(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("rehash", size), &data, |b, c| {
            b.iter(|| {
                let mut acc = 0u64;
                sngram::scan(&table, Cursor::new(c), |event| {
                    if let ScanEvent::Gram(gram) = event {
                        let mut hasher = rustc_hash::FxHasher::default();
                        gram.bytes.hash(&mut hasher);
                        acc ^= hasher.finish();
                    }
                })
                .expect("scan succeeds");
                black_box(acc)
            });
        });
        group.bench_with_input(BenchmarkId::new("fused", size), &data, |b, c| {
            b.iter(|| {
                let mut acc = 0u64;
                sngram::scan(&table, Cursor::new(c), |event| {
                    if let ScanEvent::Gram(gram) = event {
                        acc ^= gram.hash;
                    }
                })
                .expect("scan succeeds");
                black_box(acc)
            });
        });
    }
    group.finish();
}

// Reports emissions/byte and distinct-grams/byte for the indexer's per-task
// memory and concurrency budget; prints once, registers no timed bench.
fn report_density(_c: &mut Criterion) {
    let table = crc32_table();
    for &size in &[4096usize, 65536, 1_048_576] {
        let data = source_code(size);
        let mut emissions = 0u64;
        let mut distinct = std::collections::HashSet::new();
        sngram::scan(&table, Cursor::new(&data), |event| {
            if let ScanEvent::Gram(gram) = event {
                emissions += 1;
                distinct.insert(gram.hash);
            }
        })
        .expect("scan succeeds");
        eprintln!(
            "density {size:>8}B: {:.3} emissions/byte, {:.3} distinct/byte, {} distinct",
            emissions as f64 / size as f64,
            distinct.len() as f64 / size as f64,
            distinct.len(),
        );
    }
}

criterion_group!(
    benches,
    bench_scan_code,
    bench_scan_prose,
    bench_scan_uniform,
    bench_scan_ascending,
    bench_weight_lookup,
    bench_pipeline,
    report_density,
);
criterion_main!(benches);
