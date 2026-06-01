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
//! **Querying** (per regex): the pattern is parsed into an AST,
//! fixed literal substrings are extracted (both prefix and suffix),
//! and each literal is decomposed into a minimal covering set of
//! sparse n-grams (a subset of the index set). These are looked up
//! in the inverted index.
//!
//! # Choosing an API
//!
//! - [`scan`] — zero-allocation callback. Use when you hash and
//!   insert grams directly into an inverted index (6x faster at 1 MB).
//! - [`index`] — collects grams into a `Vec`. Use when you need
//!   to keep grams around or iterate them multiple times.
//! - [`query`] — decomposes a regex into covering grams for lookup.

pub mod error;
pub mod pattern;
pub mod plan;

mod extract;

use sngram_types::{Content, IndexGrams, WeightTable};

use pattern::Pattern;
use plan::QueryPlan;

/// Collect all sparse n-grams from content into a `Vec`.
#[must_use]
pub fn index<'a>(table: &WeightTable, content: &Content<'a>) -> IndexGrams<'a> {
    extract::all(table, content.as_bytes())
}

/// Zero-allocation scan. Calls `emit(start, end)` per gram.
///
/// Preferred over [`index`] when grams are consumed once (e.g.
/// hashing into an inverted index). 6x faster at 1 MB input.
pub fn scan(table: &WeightTable, content: &Content<'_>, emit: impl FnMut(usize, usize)) {
    extract::scan(table, content.as_bytes(), emit);
}

/// Decompose a regex pattern into a sparse-gram [`QueryPlan`] for index lookup.
///
/// Infallible: a too-broad pattern yields [`QueryPlan::All`] and an impossible
/// one yields [`QueryPlan::None`].
#[must_use]
pub fn query(table: &WeightTable, pattern: &Pattern) -> QueryPlan {
    plan::query(table, pattern)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::QueryError;
    use sngram_types::TABLE_BINARY_SIZE;

    fn table() -> WeightTable {
        let mut buf = vec![0u8; TABLE_BINARY_SIZE];
        buf[..4].copy_from_slice(b"SPNG");
        buf[4..8].copy_from_slice(&1u32.to_le_bytes());
        for c1 in 0u8..=255 {
            for c2 in 0u8..=255 {
                let w = crc32fast::hash(&[c1, c2]);
                let idx = (usize::from(c1) << 8) | usize::from(c2);
                let off = 16 + idx * 4;
                buf[off..off + 4].copy_from_slice(&w.to_le_bytes());
            }
        }
        let crc = crc32fast::hash(&buf[16..]);
        buf[8..12].copy_from_slice(&crc.to_le_bytes());
        WeightTable::from_bytes(&buf).unwrap()
    }

    fn index_set(t: &WeightTable, doc: &[u8]) -> std::collections::HashSet<Vec<u8>> {
        index(t, &Content::new(doc))
            .iter()
            .map(|g| g.as_bytes().to_vec())
            .collect()
    }

    // Weights that strictly decrease along the byte run 1,2,3,... so the index
    // scan's stack only ever grows — the worst case for a bounded stack.
    fn monotonic_table() -> WeightTable {
        let mut buf = vec![0u8; TABLE_BINARY_SIZE];
        buf[..4].copy_from_slice(b"SPNG");
        buf[4..8].copy_from_slice(&1u32.to_le_bytes());
        for v in 1u16..200 {
            let idx = ((v << 8) | (v + 1)) as usize;
            let off = 16 + idx * 4;
            let w = 1_000_000u32 - u32::from(v);
            buf[off..off + 4].copy_from_slice(&w.to_le_bytes());
        }
        let crc = crc32fast::hash(&buf[16..]);
        buf[8..12].copy_from_slice(&crc.to_le_bytes());
        WeightTable::from_bytes(&buf).unwrap()
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
                idx.contains(g),
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
                idx.contains(g),
                "FALSE NEGATIVE past stack cap: {:?} missing from index",
                String::from_utf8_lossy(g),
            );
        }
    }

    fn set_weight(buf: &mut [u8], c1: u8, c2: u8, w: u32) {
        let idx = (usize::from(c1) << 8) | usize::from(c2);
        let off = 16 + idx * 4;
        buf[off..off + 4].copy_from_slice(&w.to_le_bytes());
    }

    // One very rare border bigram (200,1) followed by an increasing run, so the
    // covering hull holds position 0 while later positions drain — producing a
    // ~98-byte covering gram and forcing `cover`'s max-length front-eviction.
    fn increasing_table() -> WeightTable {
        let mut buf = vec![0u8; TABLE_BINARY_SIZE];
        buf[..4].copy_from_slice(b"SPNG");
        buf[4..8].copy_from_slice(&1u32.to_le_bytes());
        set_weight(&mut buf, 200, 1, 2_000_000);
        for k in 1u8..130 {
            set_weight(&mut buf, k, k + 1, u32::from(k));
        }
        let crc = crc32fast::hash(&buf[16..]);
        buf[8..12].copy_from_slice(&crc.to_le_bytes());
        WeightTable::from_bytes(&buf).unwrap()
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
                idx.contains(g),
                "FALSE NEGATIVE on long literal: gram of len {} missing from index",
                g.len(),
            );
        }
    }

    // -- index: edge cases --

    #[test]
    fn empty_content_returns_empty() {
        let grams = index(&table(), &Content::new(b""));
        assert!(grams.is_empty());
    }

    #[test]
    fn one_byte_returns_empty() {
        let grams = index(&table(), &Content::new(b"x"));
        assert!(grams.is_empty());
    }

    #[test]
    fn two_bytes_returns_empty() {
        let grams = index(&table(), &Content::new(b"ab"));
        assert!(grams.is_empty());
    }

    #[test]
    fn three_bytes_produces_grams() {
        let grams = index(&table(), &Content::new(b"abc"));
        assert!(!grams.is_empty());
    }

    // -- index: invariant --

    #[test]
    fn all_grams_have_borders_greater_than_internals() {
        let t = table();
        let content = b"fn main() { let x = foo_bar(42); }";
        let grams = index(&t, &Content::new(content));

        for gram in &grams {
            let bytes = gram.as_bytes();
            if bytes.len() <= 3 {
                continue;
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
        }
    }

    // -- index: coverage patterns --

    #[test]
    fn uniform_content_produces_grams() {
        let data = vec![b'a'; 100];
        let grams = index(&table(), &Content::new(&data));
        assert!(!grams.is_empty());
    }

    #[test]
    fn real_source_code_produces_grams() {
        let src = b"use std::collections::HashMap;\nfn main() {\n}";
        let grams = index(&table(), &Content::new(src));
        assert!(grams.len() > 1);
    }

    #[test]
    #[allow(clippy::cast_precision_loss, reason = "diagnostic ratio only")]
    fn gram_density() {
        let t = table();
        let src = b"fn main() { let x = foo_bar(42); println!(\"{x}\"); }\n";
        for &size in &[64, 256, 1024, 4096, 16384, 65536] {
            let data: Vec<u8> = (0..size).map(|i| src[i % src.len()]).collect();
            let grams = index(&t, &Content::new(&data));
            let ratio = grams.len() as f64 / size as f64;
            eprintln!("{size:>6}B -> {:>6} grams ({ratio:.2}/byte)", grams.len());
        }
    }

    // -- index: hashes are deterministic --

    #[test]
    fn hashes_are_deterministic() {
        let t = table();
        let content = Content::new(b"hello world");
        let h1: Vec<u64> = index(&t, &content).hashes().collect();
        let h2: Vec<u64> = index(&t, &content).hashes().collect();
        assert_eq!(h1, h2);
    }

    // -- query: literal extraction --

    #[test]
    fn literal_pattern_extracts_grams() {
        let t = table();
        let pat = Pattern::new("MAX_FILE_SIZE").unwrap();
        assert!(matches!(query(&t, &pat), QueryPlan::And { .. }));
    }

    // -- query: too broad yields All (infallible, like codesearch) --

    #[test]
    fn pure_wildcard_is_all() {
        let pat = Pattern::new(".*").unwrap();
        assert_eq!(query(&table(), &pat), QueryPlan::All);
    }

    #[test]
    fn pure_class_is_all() {
        let pat = Pattern::new(r"[a-z]+").unwrap();
        assert_eq!(query(&table(), &pat), QueryPlan::All);
    }

    #[test]
    fn short_literal_is_all() {
        let pat = Pattern::new("ab").unwrap();
        assert_eq!(query(&table(), &pat), QueryPlan::All);
    }

    // -- pattern: parse errors --

    #[test]
    fn oversized_pattern_returns_too_long() {
        let long = "a".repeat(5000);
        let err = Pattern::new(&long).unwrap_err();
        assert!(matches!(err, QueryError::PatternTooLong { .. }));
    }

    #[test]
    fn invalid_regex_returns_error() {
        let err = Pattern::new("(unclosed").unwrap_err();
        assert!(matches!(err, QueryError::InvalidRegex(_)));
    }

}
