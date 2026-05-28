#![allow(
    missing_docs,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::cast_possible_truncation,
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

const PAIR_COUNT: usize = 256 * 256;

struct Counter {
    counts: Box<[AtomicU64; PAIR_COUNT]>,
}

impl Counter {
    fn new() -> Self {
        let counts: Box<[AtomicU64; PAIR_COUNT]> = (0..PAIR_COUNT)
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice()
            .try_into()
            .unwrap();
        Self { counts }
    }

    #[inline]
    fn process(&self, content: &[u8]) {
        for pair in content.windows(2) {
            let idx = usize::from(pair[0]) << 8 | usize::from(pair[1]);
            self.counts[idx].fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn source_code(size: usize) -> Vec<u8> {
    let src = b"fn main() { let x = foo_bar(42); println!(\"{x}\"); }\n";
    (0..size).map(|i| src[i % src.len()]).collect()
}

const SIZES: &[usize] = &[256, 1024, 4096, 16384, 65536, 262_144, 1_048_576];

fn bench_process_single(c: &mut Criterion) {
    let counter = Counter::new();
    let mut group = c.benchmark_group("counter/single");

    for &size in SIZES {
        let data = source_code(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, d| {
            b.iter(|| counter.process(d));
        });
    }
    group.finish();
}

fn bench_process_concurrent(c: &mut Criterion) {
    let mut group = c.benchmark_group("counter/concurrent");

    for threads in [1, 2, 4, 8] {
        let size = 65536;
        let data = source_code(size);
        let total = (size * threads) as u64;
        group.throughput(Throughput::Bytes(total));
        group.bench_with_input(
            BenchmarkId::new("threads", threads),
            &data,
            |b, d| {
                let counter = Arc::new(Counter::new());
                b.iter(|| {
                    std::thread::scope(|s| {
                        for _ in 0..threads {
                            let c = &counter;
                            s.spawn(|| c.process(d));
                        }
                    });
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_process_single, bench_process_concurrent);
criterion_main!(benches);
