#![allow(
    missing_docs,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::excessive_nesting,
    clippy::unwrap_used,
    clippy::expect_used
)]

use std::collections::HashSet;

use divan::{Bencher, counter::BytesCount};
use sngram::WeightTable;

const SIZES: &[usize] = &[64, 256, 1024, 4096, 16384, 65536, 262_144, 1_048_576];
const SMALL: &[usize] = &[256, 4096, 65536];
const PIPELINE_SIZES: &[usize] = &[65_536, 1_048_576];
const DENSITY_SIZES: &[usize] = &[4096, 65536, 1_048_576];

fn main() {
    report_density();
    divan::main();
}

fn crc32_table() -> WeightTable {
    WeightTable::from_weight_fn(|first, second| crc32fast::hash(&[first, second]))
}

fn repeated(size: usize, text: &[u8]) -> Vec<u8> {
    (0..size).map(|index| text[index % text.len()]).collect()
}

fn source_code(size: usize) -> Vec<u8> {
    repeated(
        size,
        b"fn main() { let x = foo_bar(42); println!(\"{x}\"); }\n",
    )
}

fn prose(size: usize) -> Vec<u8> {
    repeated(size, b"The quick brown fox jumps over the lazy dog. ")
}

fn ascending(size: usize) -> Vec<u8> {
    (0..size).map(|index| 32 + (index % 95) as u8).collect()
}

fn count_grams(table: &WeightTable, data: &[u8]) -> u64 {
    let mut count = 0;
    sngram::scan(table, data, |_| count += 1);
    count
}

fn bench_scan(bencher: Bencher, data: &[u8]) {
    let table = crc32_table();
    let bytes = BytesCount::new(data.len());
    bencher
        .counter(bytes)
        .bench_local(|| count_grams(&table, data));
}

#[divan::bench(args = SIZES)]
fn scan_code(bencher: Bencher, size: usize) {
    let data = source_code(size);
    bench_scan(bencher, &data);
}

#[divan::bench(args = SMALL)]
fn scan_prose(bencher: Bencher, size: usize) {
    let data = prose(size);
    bench_scan(bencher, &data);
}

#[divan::bench(args = SMALL)]
fn scan_uniform(bencher: Bencher, size: usize) {
    let data = vec![b'a'; size];
    bench_scan(bencher, &data);
}

#[divan::bench(args = SMALL)]
fn scan_ascending(bencher: Bencher, size: usize) {
    let data = ascending(size);
    bench_scan(bencher, &data);
}

// one lookup is below the resolution of CPU simulation, so sweep the whole pair space
#[divan::bench]
fn weight_lookup(bencher: Bencher) {
    let table = crc32_table();
    bencher.bench_local(|| {
        let mut total = 0u64;
        for first in 0..=u8::MAX {
            for second in 0..=u8::MAX {
                let weight = table.weight(divan::black_box(first), divan::black_box(second));
                total += u64::from(weight);
            }
        }
        total
    });
}

#[divan::bench(args = PIPELINE_SIZES)]
fn pipeline_code_keys(bencher: Bencher, size: usize) {
    let table = crc32_table();
    let data = source_code(size);
    bencher
        .counter(BytesCount::new(size))
        .bench_local(|| pipeline_keys(&table, &data));
}

fn pipeline_keys(table: &WeightTable, data: &[u8]) -> u64 {
    let mut keys = 0;
    sngram::scan(table, data, |gram| keys ^= gram.key.value());
    divan::black_box(keys)
}

fn report_density() {
    let table = crc32_table();
    for &size in DENSITY_SIZES {
        print_density(&table, size);
    }
}

fn print_density(table: &WeightTable, size: usize) {
    let data = source_code(size);
    let mut emissions = 0u64;
    let mut distinct = HashSet::new();
    sngram::scan(table, &data, |gram| {
        emissions += 1;
        distinct.insert(gram.key.value());
    });
    eprintln!(
        "density {size:>8}B: {:.3} emissions/byte, {:.3} distinct/byte, {} distinct",
        emissions as f64 / size as f64,
        distinct.len() as f64 / size as f64,
        distinct.len(),
    );
}
