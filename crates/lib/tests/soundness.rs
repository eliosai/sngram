//! End-to-end soundness: the gram plan must never drop a real match.
//!
//! For each pattern and document, if the real regex (an independent oracle)
//! matches the document, the document's sparse index grams must satisfy the
//! [`QueryPlan`]. A failure is a false negative: a matching file the prefilter
//! would wrongly skip. Everything here runs through the public API.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "tests assert by panicking; the fixture indexes a fixed-shape buffer"
)]

use std::collections::HashSet;

use sngram::{Pattern, QueryPlan, query, scan};
use sngram_types::{Content, TABLE_BINARY_SIZE, WeightTable};

/// A deterministic weight table: each byte pair hashed to a varied weight, so
/// the sparse hull is non-trivial.
fn weight_table() -> WeightTable {
    let mut buf = vec![0u8; TABLE_BINARY_SIZE];
    buf[..4].copy_from_slice(b"SPNG");
    buf[4..8].copy_from_slice(&1u32.to_le_bytes());
    for c1 in 0u8..=255 {
        for c2 in 0u8..=255 {
            let w = crc32fast::hash(&[c1, c2]);
            let idx = (usize::from(c1) << 8) | usize::from(c2);
            buf[16 + idx * 4..16 + idx * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
    }
    let crc = crc32fast::hash(&buf[16..]);
    buf[8..12].copy_from_slice(&crc.to_le_bytes());
    WeightTable::from_bytes(&buf).unwrap()
}

/// Evaluate a plan against the grams a document indexed to.
fn satisfies(plan: &QueryPlan, grams: &HashSet<Vec<u8>>) -> bool {
    match plan {
        QueryPlan::All => true,
        QueryPlan::None => false,
        QueryPlan::And { grams: g, sub } => {
            g.iter().all(|x| grams.contains(x.as_bytes()))
                && sub.iter().all(|s| satisfies(s, grams))
        }
        QueryPlan::Or { grams: g, sub } => {
            g.iter().any(|x| grams.contains(x.as_bytes()))
                || sub.iter().any(|s| satisfies(s, grams))
        }
    }
}

fn index_grams(t: &WeightTable, doc: &[u8]) -> HashSet<Vec<u8>> {
    let mut grams = HashSet::new();
    scan(t, &Content::new(doc), |s, e, _| {
        grams.insert(doc[s..e].to_vec());
    });
    grams
}

/// Assert no document the oracle matches is rejected by the plan.
fn assert_no_false_negative(t: &WeightTable, re: &str, docs: &[&[u8]], indexed: &[HashSet<Vec<u8>>]) {
    let plan = query(t, &Pattern::new(re).expect("pattern parses"));
    if plan == QueryPlan::All {
        return; // too broad to prefilter; the caller scans instead
    }
    let oracle = regex_lite::Regex::new(re).expect("oracle parses pattern");
    for (doc, grams) in docs.iter().zip(indexed) {
        let text = String::from_utf8_lossy(doc);
        if oracle.is_match(&text) {
            assert!(
                satisfies(&plan, grams),
                "FALSE NEGATIVE: {re:?} matches {text:?} but the plan rejects it",
            );
        }
    }
}

fn corpus() -> Vec<&'static [u8]> {
    vec![
        b"pub async fn read_content(hash: Hash) -> Result<Bytes, Error> {".as_slice(),
        b"    let max_file_size = 4 * 1024 * 1024; // 4 MiB".as_slice(),
        b"fn main() { println!(\"hello, world\"); }".as_slice(),
        b"struct ContentStore { bucket: Bucket, table: WeightTable }".as_slice(),
        b"impl WeightTable { fn weight(&self, a: u8, b: u8) -> u32 {} }".as_slice(),
        b"return Err(QueryError::NoLiterals);".as_slice(),
        b"const MAX_FILE_SIZE: usize = 4_194_304;".as_slice(),
        b"let mut grams = Vec::with_capacity(n * 2);".as_slice(),
        b"// the quick brown fox jumps over the lazy dog".as_slice(),
        b"SELECT grams FROM content_ngrams WHERE grams @> ARRAY[1,2,3];".as_slice(),
    ]
}

const PATTERNS: &[&str] = &[
    "fn main",
    "async fn",
    "MAX_FILE_SIZE",
    "ContentStore",
    "WeightTable",
    "return Err",
    "let mut",
    "content_ngrams",
    "the quick brown",
    "(MAX|MIN)_FILE_SIZE",
    "fn.*Result",
    "impl.*WeightTable",
    "Content(Store|Ngrams)",
    "grams @> ARRAY",
    "(?i)weighttable",
    "println",
    "1024 \\* 1024",
];

#[test]
fn plan_never_misses_a_real_match_on_realistic_code() {
    let t = weight_table();
    let docs = corpus();
    let indexed: Vec<_> = docs.iter().map(|d| index_grams(&t, d)).collect();
    for &re in PATTERNS {
        assert_no_false_negative(&t, re, &docs, &indexed);
    }
}

#[test]
fn plan_never_misses_on_exhaustive_small_alphabet_sweep() {
    let t = weight_table();
    let owned = words(6);
    let docs: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    let indexed: Vec<_> = docs.iter().map(|d| index_grams(&t, d)).collect();
    for re in sweep_patterns() {
        if Pattern::new(&re).is_ok() {
            assert_no_false_negative(&t, &re, &docs, &indexed);
        }
    }
}

/// All non-empty strings over {a,b,c} up to `max` bytes.
fn words(max: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut frontier = vec![Vec::new()];
    for _ in 0..max {
        let mut next = Vec::new();
        for w in &frontier {
            for &c in b"abc" {
                let mut n = w.clone();
                n.push(c);
                out.push(n.clone());
                next.push(n);
            }
        }
        frontier = next;
    }
    out
}

/// Short regexes exercising literals, alternation, concatenation, repetition,
/// and classes over {a,b,c}.
fn sweep_patterns() -> Vec<String> {
    let mut pats: Vec<String> = [
        "abc", "abca", "a.c", "ab.ab", "abc|bca", "a(bc|ca)b", "abc.*abc", "a+bc", "ab?cabc",
        "[ab]cabc", "abcabc", "(abc)+", "ca(b|c)ca",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    for &a in b"abc" {
        for &b in b"abc" {
            for &c in b"abc" {
                pats.push(String::from_utf8(vec![a, b, c, a, b, c]).unwrap());
            }
        }
    }
    pats
}
