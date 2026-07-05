#![allow(
    missing_docs,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::cast_possible_truncation,
    clippy::excessive_nesting
)]

//! Benchmarks the real training ingest path: `LocalTally::count_buffer` is the
//! per-byte hot loop every training byte passes through, and `merge` is the
//! once-per-batch fold into the shared counter.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use sngram::learn::{BigramCounter, LocalTally};

fn source_code(size: usize) -> Vec<u8> {
    let src = b"fn main() { let x = foo_bar(42); println!(\"{x}\"); }\n";
    (0..size).map(|i| src[i % src.len()]).collect()
}

/// Realistic mixed corpus: code-like text salted with pseudo-random bytes so
/// the histogram working set is wider than pure source.
fn mixed(size: usize) -> Vec<u8> {
    let src = source_code(size);
    let mut state = 0x9E37_79B9_u32;
    src.iter()
        .map(|&b| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            if state.is_multiple_of(5) {
                (state >> 24) as u8
            } else {
                b
            }
        })
        .collect()
}

const SIZES: &[usize] = &[4096, 65536, 1_048_576, 16 * 1_048_576];

fn bench_count_buffer(c: &mut Criterion) {
    let mut group = c.benchmark_group("counter/count_buffer");

    for &size in SIZES {
        for (name, data) in [("code", source_code(size)), ("mixed", mixed(size))] {
            group.throughput(Throughput::Bytes(size as u64));
            group.bench_with_input(BenchmarkId::new(name, size), &data, |b, d| {
                let mut tally = LocalTally::new();
                b.iter(|| {
                    tally.count_buffer(black_box(d));
                    black_box(tally.bytes())
                });
            });
        }
    }
    group.finish();
}

fn bench_merge(c: &mut Criterion) {
    let mut tally = LocalTally::new();
    tally.count_buffer(&source_code(1_048_576));
    let counter = BigramCounter::new();
    c.bench_function("counter/merge", |b| {
        b.iter(|| counter.merge(black_box(&tally)));
    });
}

fn bench_concurrent_merge(c: &mut Criterion) {
    let mut group = c.benchmark_group("counter/concurrent");

    for threads in [1usize, 2, 4, 8] {
        let size = 1_048_576;
        let data = source_code(size);
        let total = (size * threads) as u64;
        group.throughput(Throughput::Bytes(total));
        group.bench_with_input(BenchmarkId::new("threads", threads), &data, |b, d| {
            let counter = Arc::new(BigramCounter::new());
            b.iter(|| {
                std::thread::scope(|s| {
                    for _ in 0..threads {
                        let c = &counter;
                        s.spawn(move || {
                            let mut tally = LocalTally::new();
                            tally.count_buffer(d);
                            c.merge(&tally);
                        });
                    }
                });
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_count_buffer,
    bench_merge,
    bench_concurrent_merge
);
criterion_main!(benches);
