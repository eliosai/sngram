#![allow(
    missing_docs,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]

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

fn bench_index_code(c: &mut Criterion) {
    let table = crc32_table();
    let mut group = c.benchmark_group("index/code");

    for &size in SIZES {
        let data = source_code(size);
        let content = Content::new(&data);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &content, |b, c| {
            b.iter(|| sngram::index(&table, c));
        });
    }
    group.finish();
}

fn bench_index_prose(c: &mut Criterion) {
    let table = crc32_table();
    let mut group = c.benchmark_group("index/prose");

    for &size in SIZES {
        let data = prose(size);
        let content = Content::new(&data);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &content, |b, c| {
            b.iter(|| sngram::index(&table, c));
        });
    }
    group.finish();
}

fn bench_index_uniform(c: &mut Criterion) {
    let table = crc32_table();
    let mut group = c.benchmark_group("index/uniform");

    for &size in SMALL {
        let data = vec![b'a'; size];
        let content = Content::new(&data);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &content, |b, c| {
            b.iter(|| sngram::index(&table, c));
        });
    }
    group.finish();
}

fn bench_index_ascending(c: &mut Criterion) {
    let table = crc32_table();
    let mut group = c.benchmark_group("index/ascending");

    for &size in SMALL {
        let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        let content = Content::new(&data);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &content, |b, c| {
            b.iter(|| sngram::index(&table, c));
        });
    }
    group.finish();
}

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
                sngram::scan(&table, c, |_, _| count += 1);
                count
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
        scanner.push(part, |_| count += 1);
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
        sngram::scan(&table, &content, |start, end| {
            emissions += 1;
            distinct.insert(content.as_bytes()[start..end].to_vec());
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
    bench_index_code,
    bench_index_prose,
    bench_index_uniform,
    bench_index_ascending,
    bench_weight_lookup,
    bench_scan_code,
    bench_scan_stream_code,
    report_density,
);
criterion_main!(benches);
