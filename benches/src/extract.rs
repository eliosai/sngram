#![allow(
    missing_docs,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
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

criterion_group!(
    benches,
    bench_index_code,
    bench_index_prose,
    bench_index_uniform,
    bench_index_ascending,
    bench_weight_lookup,
    bench_scan_code,
);
criterion_main!(benches);
