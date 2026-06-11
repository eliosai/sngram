//! Streaming equivalence: `StreamScanner` over any chunking of a document must
//! emit exactly the grams `scan` emits over the whole document, in order. A
//! divergence is a corrupted index, so the sweep is exhaustive over chunk size.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    reason = "tests assert by panicking and index fixed-shape weight tables"
)]

use sngram::StreamScanner;
use sngram_types::{Content, TABLE_BINARY_SIZE, WeightTable};

fn set_weight(buf: &mut [u8], c1: u8, c2: u8, w: u32) {
    let idx = (usize::from(c1) << 8) | usize::from(c2);
    buf[16 + idx * 4..16 + idx * 4 + 4].copy_from_slice(&w.to_le_bytes());
}

fn finish(mut buf: Vec<u8>) -> WeightTable {
    buf[..4].copy_from_slice(b"SPNG");
    buf[4..8].copy_from_slice(&1u32.to_le_bytes());
    let crc = crc32fast::hash(&buf[16..]);
    buf[8..12].copy_from_slice(&crc.to_le_bytes());
    WeightTable::from_bytes(&buf).unwrap()
}

/// Every byte pair gets a varied weight, so the sparse hull is non-trivial.
fn crc_table() -> WeightTable {
    let mut buf = vec![0u8; TABLE_BINARY_SIZE];
    for c1 in 0u16..256 {
        for c2 in 0u16..256 {
            set_weight(&mut buf, c1 as u8, c2 as u8, crc32fast::hash(&[c1 as u8, c2 as u8]));
        }
    }
    finish(buf)
}

/// Strictly decreasing weights along 1,2,3,... so the stack only grows: the
/// worst case for the bounded stack and its overflow eviction.
fn monotonic_table() -> WeightTable {
    let mut buf = vec![0u8; TABLE_BINARY_SIZE];
    for v in 1u16..200 {
        set_weight(&mut buf, v as u8, (v + 1) as u8, 1_000_000 - u32::from(v));
    }
    finish(buf)
}

/// One very rare border pair then an increasing run, producing long grams that
/// exercise the covering front-eviction path past 50 bytes.
fn increasing_table() -> WeightTable {
    let mut buf = vec![0u8; TABLE_BINARY_SIZE];
    set_weight(&mut buf, 200, 1, 2_000_000);
    for k in 1u8..130 {
        set_weight(&mut buf, k, k + 1, u32::from(k));
    }
    finish(buf)
}

fn batch(table: &WeightTable, doc: &[u8]) -> Vec<(Vec<u8>, u64)> {
    let mut out = Vec::new();
    sngram::scan(table, &Content::new(doc), |start, end, hash| {
        out.push((doc[start..end].to_vec(), hash));
    });
    out
}

fn streamed(table: &WeightTable, doc: &[u8], chunk: usize) -> Vec<(Vec<u8>, u64)> {
    let mut out = Vec::new();
    let mut scanner = StreamScanner::new(table);
    for part in doc.chunks(chunk) {
        scanner.push(part, |gram, hash| out.push((gram.to_vec(), hash)));
    }
    scanner.finish();
    out
}

/// The streaming emission equals the batch emission for every chunk size,
/// including the single-byte feed, the worst boundary case.
fn assert_equivalent(table: &WeightTable, doc: &[u8]) {
    let expected = batch(table, doc);
    for chunk in 1..=doc.len().max(1) {
        assert_eq!(
            streamed(table, doc, chunk),
            expected,
            "chunk size {chunk} diverged from batch for a {}-byte document",
            doc.len(),
        );
    }
}

fn corpus() -> Vec<Vec<u8>> {
    let lines: &[&[u8]] = &[
        b"",
        b"a",
        b"ab",
        b"abc",
        b"fn main() { println!(\"hello, world\"); }",
        b"pub async fn read_content(hash: Hash) -> Result<Bytes, Error> {",
        b"const MAX_FILE_SIZE: usize = 4_194_304;",
        b"SELECT grams FROM content_ngrams WHERE grams @> ARRAY[1,2,3];",
        b"the quick brown fox jumps over the lazy dog",
    ];
    lines.iter().map(|l| l.to_vec()).collect()
}

#[test]
fn stream_matches_batch_on_realistic_code() {
    let table = crc_table();
    for doc in corpus() {
        assert_equivalent(&table, &doc);
    }
}

#[test]
fn stream_matches_batch_across_compaction() {
    let table = crc_table();
    let src = b"fn max_file_size() -> u64 { 4 * 1024 * 1024 }\n";
    let doc: Vec<u8> = (0..600).map(|i| src[i % src.len()]).collect();
    assert_equivalent(&table, &doc);
}

#[test]
fn stream_matches_batch_on_growing_stack() {
    let table = monotonic_table();
    let doc: Vec<u8> = (1u8..=200).collect();
    assert_equivalent(&table, &doc);
}

#[test]
fn stream_matches_batch_on_long_grams() {
    let table = increasing_table();
    let mut doc = vec![200u8];
    doc.extend(1u8..=130);
    assert_equivalent(&table, &doc);
}

#[test]
fn stream_matches_batch_on_repeated_long_input() {
    let table = monotonic_table();
    let mut doc = Vec::new();
    for _ in 0..6 {
        doc.extend(1u8..=120);
    }
    assert_equivalent(&table, &doc);
}
