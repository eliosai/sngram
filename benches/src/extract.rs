#![allow(
    missing_docs,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::excessive_nesting,
    clippy::too_many_lines,
    clippy::unwrap_used
)]

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use sngram_types::{Content, WeightTable};

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

fn source_code(size: usize) -> Vec<u8> {
    let src = b"fn main() { let x = foo_bar(42); println!(\"{x}\"); }\n";
    (0..size).map(|i| src[i % src.len()]).collect()
}

fn prose(size: usize) -> Vec<u8> {
    let txt = b"The quick brown fox jumps over the lazy dog. ";
    (0..size).map(|i| txt[i % txt.len()]).collect()
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
        let content = Content::new(&data);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &content, |b, c| {
            b.iter(|| {
                let mut count = 0u64;
                sngram::scan(&table, c, |_, _, _| count += 1);
                count
            });
        });
    }
    group.finish();
}

fn bench_scan_prose(c: &mut Criterion) {
    let table = crc32_table();
    let mut group = c.benchmark_group("scan/prose");

    for &size in SMALL {
        let data = prose(size);
        let content = Content::new(&data);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &content, |b, c| {
            b.iter(|| {
                let mut count = 0u64;
                sngram::scan(&table, c, |_, _, _| count += 1);
                count
            });
        });
    }
    group.finish();
}

fn bench_scan_uniform(c: &mut Criterion) {
    let table = crc32_table();
    let mut group = c.benchmark_group("scan/uniform");

    for &size in SMALL {
        let data = vec![b'a'; size];
        let content = Content::new(&data);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &content, |b, c| {
            b.iter(|| {
                let mut count = 0u64;
                sngram::scan(&table, c, |_, _, _| count += 1);
                count
            });
        });
    }
    group.finish();
}

fn bench_scan_ascending(c: &mut Criterion) {
    let table = crc32_table();
    let mut group = c.benchmark_group("scan/ascending");

    for &size in SMALL {
        let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        let content = Content::new(&data);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &content, |b, c| {
            b.iter(|| {
                let mut count = 0u64;
                sngram::scan(&table, c, |_, _, _| count += 1);
                count
            });
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
        let content = Content::new(&data);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("rehash", size), &content, |b, c| {
            b.iter(|| {
                let mut acc = 0u64;
                sngram::scan(&table, c, |s, e, _| {
                    let mut hasher = rustc_hash::FxHasher::default();
                    data[s..e].hash(&mut hasher);
                    acc ^= hasher.finish();
                });
                black_box(acc)
            });
        });
        group.bench_with_input(BenchmarkId::new("fused", size), &content, |b, c| {
            b.iter(|| {
                let mut acc = 0u64;
                sngram::scan(&table, c, |_, _, h| acc ^= h);
                black_box(acc)
            });
        });
    }
    group.finish();
}

// Chunk sizes a streaming reader hands the scanner; the 64 B feed stresses the
// per-chunk boundary path, the larger ones approach the batch hot loop.
const CHUNKS: &[usize] = &[64, 4096, 65536];

fn stream_count(table: &WeightTable, data: &[u8], chunk: usize) -> u64 {
    let mut count = 0u64;
    let mut scanner = sngram::StreamScanner::new(table);
    for part in data.chunks(chunk) {
        scanner.push(part, |_, _| count += 1);
    }
    scanner.finish();
    count
}

fn bench_scan_stream_code(c: &mut Criterion) {
    let table = crc32_table();
    let mut group = c.benchmark_group("scan_stream/code");

    for &size in SIZES {
        let data = source_code(size);
        for &chunk in CHUNKS {
            group.throughput(Throughput::Bytes(size as u64));
            group.bench_with_input(
                BenchmarkId::new(format!("chunk_{chunk}"), size),
                &data,
                |b, data| b.iter(|| stream_count(&table, data, chunk)),
            );
        }
    }
    group.finish();
}

// Reports emissions/byte and distinct-grams/byte for the indexer's per-task
// memory and concurrency budget; prints once, registers no timed bench.
fn report_density(_c: &mut Criterion) {
    let table = crc32_table();
    for &size in &[4096usize, 65536, 1_048_576] {
        let data = source_code(size);
        let content = Content::new(&data);
        let mut emissions = 0u64;
        let mut distinct = std::collections::HashSet::new();
        sngram::scan(&table, &content, |_, _, h| {
            emissions += 1;
            distinct.insert(h);
        });
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
    bench_scan_stream_code,
    report_density,
);
criterion_main!(benches);
