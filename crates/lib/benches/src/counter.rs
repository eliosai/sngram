#![allow(
    missing_docs,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::cast_possible_truncation,
    clippy::excessive_nesting
)]

use std::sync::Arc;

use divan::{Bencher, counter::BytesCount};
use sngram::learn::BigramCounter;

const SIZES: &[usize] = &[4096, 65536, 1_048_576, 16 * 1_048_576];
const THREADS: &[usize] = &[1, 2, 4, 8];

fn main() {
    divan::main();
}

fn source_code(size: usize) -> Vec<u8> {
    let source = b"fn main() { let x = foo_bar(42); println!(\"{x}\"); }\n";
    (0..size)
        .map(|index| source[index % source.len()])
        .collect()
}

fn mixed(size: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_u32;
    source_code(size)
        .into_iter()
        .map(|byte| mixed_byte(byte, &mut state))
        .collect()
}

const fn mixed_byte(byte: u8, state: &mut u32) -> u8 {
    *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    if state.is_multiple_of(5) {
        (*state >> 24) as u8
    } else {
        byte
    }
}

fn process_batch(bencher: Bencher, data: &[u8]) {
    let counter = BigramCounter::new();
    bencher
        .counter(BytesCount::new(data.len()))
        .bench_local(|| counter.process_batch(core::iter::once(divan::black_box(data))));
}

#[divan::bench(args = SIZES)]
fn process_batch_code(bencher: Bencher, size: usize) {
    let data = source_code(size);
    process_batch(bencher, &data);
}

#[divan::bench(args = SIZES)]
fn process_batch_mixed(bencher: Bencher, size: usize) {
    let data = mixed(size);
    process_batch(bencher, &data);
}

#[divan::bench]
fn counter_merge(bencher: Bencher) {
    let staging = BigramCounter::new();
    staging.process(&source_code(1_048_576));
    let counter = BigramCounter::new();
    bencher.bench_local(|| counter.merge(divan::black_box(&staging)));
}

#[divan::bench(args = THREADS)]
fn concurrent_merge(bencher: Bencher, threads: usize) {
    let data = source_code(1_048_576);
    let counter = Arc::new(BigramCounter::new());
    bencher
        .counter(BytesCount::new(data.len() * threads))
        .bench_local(|| merge_in_threads(&counter, &data, threads));
}

fn merge_in_threads(counter: &Arc<BigramCounter>, data: &[u8], threads: usize) {
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let staging = BigramCounter::new();
                staging.process(data);
                counter.merge(&staging);
            });
        }
    });
}
