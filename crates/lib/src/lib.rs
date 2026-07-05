//! Sparse n-gram extraction for code search indexing.
//!
//! Stateless, `Send + Sync`, zero contention.
//!
//! # Algorithm
//!
//! A weight table assigns a u32 weight to every byte pair (bigram).
//! Rare pairs get high weights, common pairs get low weights.
//!
//! **Indexing** (per document): a monotonic stack scans all byte
//! pairs left-to-right. Substrings where both border weights are
//! strictly greater than all internal weights are emitted as
//! sparse n-grams. These go into an inverted index keyed by hash.
//!
//! **Querying** (per regex): the pattern's HIR is folded into a
//! conservative boolean query over gram presence. Literals cover to
//! the grams the scan is guaranteed to emit for them (maximal for a
//! lone literal, minimal per branch for wide variant sets), which are
//! looked up in the inverted index.
//!
//! # API
//!
//! - [`scan`] extracts sparse n-grams from one document under explicit scan
//!   options.
//! - [`query`] decomposes one or more patterns under explicit verifier and
//!   index-format options.
//! - `learn` module (feature `learn`) trains fresh weight tables.

#[cfg(feature = "learn")]
pub mod learn;

mod extract;
mod plan;
mod types;

pub use sngram_types::{Content, Gram, HashKey, WeightTable};
pub use types::{
    DfStats, GramSpace, PlannedQuery, QueryCase, QueryError, QueryOptions, QueryPlan, QuerySyntax,
    ScanError, ScanOptions, ScannedGram,
};

/// Compiles the README's examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

/// Extract sparse n-grams from one in-memory document.
///
/// Each emitted [`ScannedGram`] carries the gram bytes in the selected scan
/// space, its scanned-stream span, and its 64-bit hash key.
///
/// # Errors
///
/// Currently infallible for in-memory content; the result leaves room for
/// future scan backends without adding another public entry point.
#[allow(
    clippy::indexing_slicing,
    reason = "the internal scanner emits spans bounded by the same content slice"
)]
pub fn scan(
    table: &WeightTable,
    content: &Content<'_>,
    opts: ScanOptions,
    mut emit: impl for<'a> FnMut(ScannedGram<'a>),
) -> Result<(), ScanError> {
    let bytes = content.as_bytes();
    if opts == ScanOptions::default() && u32::try_from(bytes.len()).is_ok() {
        extract::scan(table, bytes, |start, end, hash| {
            emit(ScannedGram {
                bytes: &bytes[start..end],
                start,
                end,
                hash,
            });
        });
    } else {
        extract::scan_options(table, bytes, opts, emit);
    }
    Ok(())
}

/// Decompose one or more patterns into a sparse-gram query plan.
///
/// [`QueryOptions`] carries both verifier semantics and index-format settings,
/// so the returned [`PlannedQuery`] names the gram space its hashes must use.
///
/// # Errors
///
/// Returns [`QueryError`] when the joined patterns exceed the length limit
/// or fail to parse.
pub fn query<P: AsRef<str>>(
    table: &WeightTable,
    patterns: &[P],
    opts: QueryOptions,
) -> Result<PlannedQuery, QueryError> {
    plan::query(table, patterns, opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "stream")]
    use crate::types::StreamScanner;

    fn table() -> WeightTable {
        WeightTable::from_weight_fn(|c1, c2| crc32fast::hash(&[c1, c2]))
    }

    fn scan_default(t: &WeightTable, doc: &[u8], emit: impl for<'a> FnMut(ScannedGram<'a>)) {
        scan(t, &Content::new(doc), ScanOptions::default(), emit).expect("scan succeeds");
    }

    fn query_default(t: &WeightTable, pattern: &str) -> QueryPlan {
        query(t, &[pattern], QueryOptions::default())
            .expect("pattern parses")
            .plan
    }

    fn index_set(t: &WeightTable, doc: &[u8]) -> std::collections::HashSet<Vec<u8>> {
        let mut set = std::collections::HashSet::new();
        scan_default(t, doc, |gram| {
            set.insert(gram.bytes.to_vec());
        });
        set
    }

    fn gram_count(t: &WeightTable, doc: &[u8]) -> usize {
        let mut n = 0usize;
        scan_default(t, doc, |_| n += 1);
        n
    }

    // Weights that strictly decrease along the byte run 1,2,3,... so the index
    // scan's stack only ever grows — the worst case for a bounded stack.
    fn monotonic_table() -> WeightTable {
        WeightTable::from_weight_fn(|c1, c2| {
            if (1..200).contains(&u16::from(c1)) && c2 == c1 + 1 {
                1_000_000u32 - u32::from(c1)
            } else {
                0
            }
        })
    }

    // Every covering gram of a literal MUST appear in the index of any document
    // containing it, or the match is missed.
    fn assert_covering_in_index(t: &WeightTable, lit: &[u8], prefix: &[u8], suffix: &[u8]) {
        let mut doc = Vec::with_capacity(prefix.len() + lit.len() + suffix.len());
        doc.extend_from_slice(prefix);
        doc.extend_from_slice(lit);
        doc.extend_from_slice(suffix);
        let idx = index_set(t, &doc);
        for g in &crate::extract::cover_one(t, lit) {
            assert!(
                idx.contains(g.as_bytes()),
                "FALSE NEGATIVE: gram {:?} of {:?} absent from index",
                String::from_utf8_lossy(g),
                String::from_utf8_lossy(lit),
            );
        }
    }

    #[test]
    fn covering_grams_are_subset_of_index() {
        let t = table();
        let lits: &[&[u8]] = &[
            b"MAX_FILE_SIZE",
            b"the quick brown fox",
            b"alpha_beta_gamma_delta",
            b"0xDEADBEEFcafe",
            b"snake_case_identifier_name",
        ];
        let ctxs: &[(&[u8], &[u8])] = &[
            (b"", b""),
            (b"zzz", b"qqq"),
            (b"a_longer_prefix_context ", b" a_longer_suffix_context"),
        ];
        for lit in lits {
            for (p, s) in ctxs {
                assert_covering_in_index(&t, lit, p, s);
            }
        }
    }

    #[test]
    fn covering_constrains_a_long_literal() {
        let t = table();
        let cov = crate::extract::cover_one(&t, b"MAX_FILE_SIZE");
        assert!(
            !cov.is_empty(),
            "covering must produce grams for a long literal"
        );
    }

    // INDEX path: a long strictly-decreasing weight run grows the scan stack
    // without bound. If it overflows and drops recent positions, deep grams go
    // missing and matches are lost. covering ⊆ index must hold past the cap.
    #[test]
    fn index_keeps_deep_grams_past_the_stack_cap() {
        let t = monotonic_table();
        let doc: Vec<u8> = (1u8..=200).collect();
        let idx = index_set(&t, &doc);
        let deep = doc[140..175].to_vec();
        let cov = crate::extract::cover_one(&t, &deep);
        assert!(!cov.is_empty(), "covering must produce grams");
        for g in &cov {
            assert!(
                idx.contains(g.as_bytes()),
                "FALSE NEGATIVE past stack cap: {:?} missing from index",
                String::from_utf8_lossy(g),
            );
        }
    }

    // One very rare border bigram (200,1) followed by an increasing run, so the
    // covering hull holds position 0 while later positions drain — producing a
    // ~98-byte covering gram and forcing `cover`'s max-length front-eviction.
    fn increasing_table() -> WeightTable {
        WeightTable::from_weight_fn(|c1, c2| {
            if c1 == 200 && c2 == 1 {
                2_000_000
            } else if (1..130).contains(&c1) && c2 == c1 + 1 {
                u32::from(c1)
            } else {
                0
            }
        })
    }

    // QUERY path: a long literal must still decompose into covering grams that
    // are all in the index and none longer than MAX_LEN — exercising the
    // front-eviction branch that short literals never reach.
    #[test]
    fn covering_a_long_literal_stays_within_the_index() {
        let t = increasing_table();
        let mut doc = vec![200u8];
        doc.extend(1u8..=130);
        let idx = index_set(&t, &doc);
        let cov = crate::extract::cover_one(&t, &doc);
        assert!(
            cov.iter().any(|g| g.len() > 50),
            "test must exercise a long front-evicted gram",
        );
        for g in &cov {
            assert!(
                idx.contains(g.as_bytes()),
                "FALSE NEGATIVE on long literal: gram of len {} missing from index",
                g.len(),
            );
        }
    }

    // -- scan: edge cases --

    #[test]
    fn empty_content_returns_empty() {
        assert_eq!(gram_count(&table(), b""), 0);
    }

    #[test]
    fn one_byte_returns_empty() {
        assert_eq!(gram_count(&table(), b"x"), 0);
    }

    #[test]
    fn two_bytes_returns_empty() {
        assert_eq!(gram_count(&table(), b"ab"), 0);
    }

    #[test]
    fn three_bytes_produces_grams() {
        assert!(gram_count(&table(), b"abc") > 0);
    }

    // -- scan: invariant --

    #[test]
    fn all_grams_have_borders_greater_than_internals() {
        let t = table();
        let content = b"fn main() { let x = foo_bar(42); }";

        scan_default(&t, content, |gram| {
            let bytes = gram.bytes;
            if bytes.len() <= 3 {
                return;
            }
            let left = t.weight(bytes[0], bytes[1]);
            let last = bytes.len() - 1;
            let right = t.weight(bytes[last - 1], bytes[last]);

            for i in 1..bytes.len() - 2 {
                let inner = t.weight(bytes[i], bytes[i + 1]);
                assert!(
                    left >= inner && right >= inner,
                    "invariant violated in {:?}: left={left} right={right} inner={inner} at {i}",
                    String::from_utf8_lossy(bytes),
                );
            }
        });
    }

    // -- scan: coverage patterns --

    #[test]
    fn uniform_content_produces_grams() {
        let data = vec![b'a'; 100];
        assert!(gram_count(&table(), &data) > 0);
    }

    #[test]
    fn real_source_code_produces_grams() {
        let src = b"use std::collections::HashMap;\nfn main() {\n}";
        assert!(gram_count(&table(), src) > 1);
    }

    #[test]
    #[allow(clippy::cast_precision_loss, reason = "diagnostic ratio only")]
    fn gram_density() {
        let t = table();
        let src = b"fn main() { let x = foo_bar(42); println!(\"{x}\"); }\n";
        for &size in &[64, 256, 1024, 4096, 16384, 65536] {
            let data: Vec<u8> = (0..size).map(|i| src[i % src.len()]).collect();
            let n = gram_count(&t, &data);
            let ratio = n as f64 / size as f64;
            eprintln!("{size:>6}B -> {n:>6} grams ({ratio:.2}/byte)");
        }
    }

    // -- scan: emitted hashes are deterministic --

    #[test]
    fn hashes_are_deterministic() {
        let t = table();
        let content = Content::new(b"hello world");
        let collect = || {
            let mut hs = Vec::new();
            scan(&t, &content, ScanOptions::default(), |gram| {
                hs.push(gram.hash)
            })
            .expect("scan succeeds");
            hs
        };
        let h1 = collect();
        let h2 = collect();
        assert!(!h1.is_empty());
        assert_eq!(h1, h2);
    }

    // -- query: literal extraction --

    #[test]
    fn literal_pattern_extracts_grams() {
        let t = table();
        assert!(matches!(
            query_default(&t, "MAX_FILE_SIZE"),
            QueryPlan::And { .. }
        ));
    }

    // -- query: too broad yields All (infallible, like codesearch) --

    #[test]
    fn pure_wildcard_is_all() {
        assert_eq!(query_default(&table(), ".*"), QueryPlan::All);
    }

    #[test]
    fn pure_class_is_all() {
        assert_eq!(query_default(&table(), r"[a-z]+"), QueryPlan::All);
    }

    #[test]
    fn short_literal_is_all() {
        assert_eq!(query_default(&table(), "ab"), QueryPlan::All);
    }

    // -- pattern: parse errors --

    #[test]
    fn oversized_pattern_returns_too_long() {
        let long = "a".repeat(5000);
        let err = query(&table(), &[long.as_str()], QueryOptions::default()).unwrap_err();
        assert!(matches!(err, QueryError::PatternTooLong { .. }));
    }

    #[test]
    fn invalid_regex_returns_error() {
        let err = query(&table(), &["(unclosed"], QueryOptions::default()).unwrap_err();
        assert!(matches!(err, QueryError::InvalidRegex(_)));
    }

    // -- streaming: the async driver agrees with batch scan --

    #[cfg(feature = "stream")]
    fn block_on<F: core::future::Future>(future: F) -> F::Output {
        use core::task::{Context, Poll, Waker};
        let mut cx = Context::from_waker(Waker::noop());
        let mut future = core::pin::pin!(future);
        loop {
            if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
                return out;
            }
        }
    }

    #[cfg(feature = "stream")]
    #[test]
    fn stream_reader_matches_batch() {
        let t = table();
        let doc = b"pub async fn read(hash: Hash) -> Result<Bytes, Error> { todo!() }";
        let mut from_reader = Vec::new();
        let reader = tokio::io::BufReader::with_capacity(7, &doc[..]);
        let mut scanner = StreamScanner::with_options(&t, ScanOptions::default());
        block_on(scanner.index_reader(reader, |gram| {
            from_reader.push((gram.bytes.to_vec(), gram.hash));
        }))
        .unwrap();

        let mut from_scan = Vec::new();
        scan_default(&t, doc, |gram| {
            from_scan.push((gram.bytes.to_vec(), gram.hash));
        });
        assert_eq!(from_reader, from_scan);
    }
}
