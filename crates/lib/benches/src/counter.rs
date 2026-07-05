#![allow(
    missing_docs,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::cast_possible_truncation,
    clippy::excessive_nesting
)]

//! Benchmarks the public training ingest path: `process_batch` does the
//! per-byte counting work and `merge` folds a completed staging counter into
//! the shared counter.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use sngram::learn::BigramCounter;

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

fn bench_process_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("counter/process_batch");

    for &size in SIZES {
        for (name, data) in [("code", source_code(size)), ("mixed", mixed(size))] {
            group.throughput(Throughput::Bytes(size as u64));
            group.bench_with_input(BenchmarkId::new(name, size), &data, |b, d| {
                let counter = BigramCounter::new();
                b.iter(|| {
                    let bytes = counter.process_batch(core::iter::once(black_box(d.as_slice())));
                    black_box(bytes)
                });
            });
        }
    }
    group.finish();
}

fn bench_merge(c: &mut Criterion) {
    let staging = BigramCounter::new();
    staging.process(&source_code(1_048_576));
    let counter = BigramCounter::new();
    c.bench_function("counter/merge", |b| {
        b.iter(|| counter.merge(black_box(&staging)));
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
                            let staging = BigramCounter::new();
                            staging.process(d);
                            c.merge(&staging);
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
    bench_process_batch,
    bench_merge,
    bench_concurrent_merge
);
criterion_main!(benches);
