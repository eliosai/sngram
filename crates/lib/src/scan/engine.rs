//! Streaming sparse n-gram scanner.

use crate::hashing::HashKey;
use crate::{ScanSummary, ScannedGram, WeightTable};

use super::facts::SummaryBuilder;
use super::settings::ScanSettings;
use super::space::{SpaceScanner, SpanMap};

/// Whole-document scanner emitting raw and folded-supplement gram keys.
pub struct DocumentScanner<'t> {
    primary: SpaceScanner<'t>,
    folded: Option<SpaceScanner<'t>>,
    summary: SummaryBuilder,
    content_bytes: usize,
    gram_count: u32,
}

impl<'t> DocumentScanner<'t> {
    pub fn new(table: &'t WeightTable) -> Self {
        Self {
            primary: SpaceScanner::new(table, HashKey::UNKEYED, SpanMap::Document),
            folded: None,
            summary: SummaryBuilder::default(),
            content_bytes: 0,
            gram_count: 0,
        }
    }

    pub fn begin_document(&mut self, emit: &mut impl FnMut(ScannedGram)) {
        self.push_sentinel(emit);
    }

    pub fn push_content(&mut self, chunk: &[u8], emit: &mut impl FnMut(ScannedGram)) {
        if chunk.is_empty() {
            return;
        }

        self.summary.observe(chunk);
        self.content_bytes += chunk.len();
        self.push_to_spaces(chunk, emit);
    }

    pub fn finish_document(&mut self, emit: &mut impl FnMut(ScannedGram)) -> ScanSummary {
        self.push_sentinel(emit);
        self.summary.finish(self.gram_count)
    }

    fn push_sentinel(&mut self, emit: &mut impl FnMut(ScannedGram)) {
        self.push_to_spaces(&[ScanSettings::DOCUMENT_SENTINEL], emit);
    }

    // the folded space stays a bit-exact mirror of the primary space until
    // the first uppercase byte, so it only starts running at that byte
    fn push_to_spaces(&mut self, chunk: &[u8], emit: &mut impl FnMut(ScannedGram)) {
        if self.folded.is_some() {
            self.push_both(chunk, emit);
            return;
        }
        let Some(at) = first_upper_index(chunk) else {
            self.push_primary(chunk, emit);
            return;
        };
        self.push_primary(&chunk[..at], emit);
        self.folded = Some(self.primary.folded_twin());
        self.push_both(&chunk[at..], emit);
    }

    fn push_both(&mut self, chunk: &[u8], emit: &mut impl FnMut(ScannedGram)) {
        let content_bytes = self.content_bytes;
        let gram_count = &mut self.gram_count;
        self.primary.push_bytes(chunk, content_bytes, &mut |gram| {
            *gram_count = gram_count.saturating_add(1);
            emit(gram);
        });

        let Some(folded) = self.folded.as_mut() else {
            return;
        };
        folded.push_bytes(chunk, content_bytes, &mut |gram| {
            *gram_count = gram_count.saturating_add(1);
            emit(gram);
        });
    }

    fn push_primary(&mut self, chunk: &[u8], emit: &mut impl FnMut(ScannedGram)) {
        let gram_count = &mut self.gram_count;
        self.primary
            .push_bytes(chunk, self.content_bytes, &mut |gram| {
                *gram_count = gram_count.saturating_add(1);
                emit(gram);
            });
    }
}

fn first_upper_index(chunk: &[u8]) -> Option<usize> {
    chunk.iter().position(u8::is_ascii_uppercase)
}

/// A scanner over one standalone literal, emitting its raw grams with
/// literal-relative spans.
pub fn literal_scanner(table: &WeightTable) -> SpaceScanner<'_> {
    SpaceScanner::new(table, HashKey::UNKEYED, SpanMap::Literal)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::hashing::HashKey;
    use crate::{ByteRange, GramKey, ScanSummary, ScannedGram};

    use super::*;

    fn table() -> WeightTable {
        WeightTable::from_weight_fn(|c1, c2| crc32fast::hash(&[c1, c2]))
    }

    fn collect(doc: &[u8]) -> CollectedScan {
        let table = table();
        let mut grams = Vec::new();
        let summary = crate::scan::scan(&table, doc, |gram| grams.push(gram));
        CollectedScan { grams, summary }
    }

    struct CollectedScan {
        grams: Vec<ScannedGram>,
        summary: ScanSummary,
    }

    #[test]
    fn emits_keys_and_summary_count() {
        let scan = collect(b"AbcDef");

        assert!(!scan.grams.is_empty());
        assert_eq!(
            usize::try_from(scan.summary.gram_count).unwrap(),
            scan.grams.len()
        );
    }

    #[test]
    fn folded_supplement_is_skipped_for_lowercase_ascii() {
        let lower = collect(b"abcdef");
        let upper = collect(b"ABCDEF");

        assert!(
            upper.grams.len() > lower.grams.len(),
            "uppercase content should add folded supplement keys"
        );
    }

    #[test]
    fn folded_supplement_keys_hash_changed_folded_spans() {
        let doc = b"xxABCyyDEFzz";
        let scan = collect(doc);

        assert!(
            scan.grams.iter().any(|gram| {
                let byte_range = gram.span.as_range();
                byte_range.start > 0
                    && byte_range.end < doc.len()
                    && doc[byte_range.clone()].iter().any(u8::is_ascii_uppercase)
                    && gram.key == folded_key(&doc[byte_range])
            }),
            "uppercase content must emit folded supplement keys for changed interior spans",
        );
    }

    #[test]
    fn lowercase_ascii_does_not_emit_folded_supplement_keys() {
        let doc = b"xxabcyydefzz";
        let scan = collect(doc);

        assert!(
            scan.grams.iter().all(|gram| {
                let byte_range = gram.span.as_range();
                byte_range.start == 0
                    || byte_range.end == doc.len()
                    || gram.key != folded_key(&doc[byte_range])
            }),
            "unchanged lowercase spans must not be duplicated into the folded key space",
        );
    }

    #[test]
    fn boundary_spans_can_hash_virtual_sentinels() {
        let scan = collect(b"A");
        let sentinel_key = GramKey::new(HashKey::UNKEYED.hash_bytes(b"\nA\n"));

        assert!(
            scan.grams
                .iter()
                .any(|gram| gram.span == ByteRange::new(0, 1) && gram.key == sentinel_key),
            "the public content span is not always the byte range that was hashed",
        );
    }

    #[test]
    fn document_spans_stay_in_content_bounds() {
        let table = table();
        crate::scan::scan(&table, b"abc", |gram| {
            assert!(gram.span.start <= gram.span.end);
            assert!(gram.span.end <= 3);
        });
    }

    #[test]
    fn summary_reports_document_facts() {
        let scan = collect("Ab\r\né".as_bytes());

        assert_eq!(scan.summary.byte_len, "Ab\r\né".len() as u64);
        assert_eq!(scan.summary.line_count, 2);
        assert!(scan.summary.flags.has_lf());
        assert!(scan.summary.flags.has_crlf());
        assert!(scan.summary.flags.has_ascii_upper());
        assert!(scan.summary.flags.has_ascii_lower());
        assert!(scan.summary.flags.has_non_ascii());
    }

    #[test]
    fn scan_grams_do_not_depend_on_chunk_size() {
        let doc = repeated_source(9000);
        let expected = canonical_grams(collect(&doc).grams);

        for cap in [1, 2, 127, 128, 129, 895, 896, 1024] {
            let table = table();
            let mut grams = Vec::new();
            let mut scanner = DocumentScanner::new(&table);
            scanner.begin_document(&mut |gram| grams.push(gram));
            for chunk in doc.chunks(cap) {
                scanner.push_content(chunk, &mut |gram| grams.push(gram));
            }
            scanner.finish_document(&mut |gram| grams.push(gram));
            assert_eq!(canonical_grams(grams), expected, "chunk capacity {cap}");
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    struct CanonicalGram {
        key: u64,
        start: usize,
        end: usize,
    }

    fn canonical_grams(grams: Vec<ScannedGram>) -> Vec<CanonicalGram> {
        let mut canonical: Vec<_> = grams
            .into_iter()
            .map(|gram| CanonicalGram {
                key: gram.key.value(),
                start: gram.span.start,
                end: gram.span.end,
            })
            .collect();
        canonical.sort_unstable();
        canonical
    }

    fn repeated_source(len: usize) -> Vec<u8> {
        let source = b"pub fn ExampleValue42() {\n    let HTTP_ID = Some(\"AlphaBeta\");\n}\n";
        (0..len).map(|i| source[i % source.len()]).collect()
    }

    fn folded_key(bytes: &[u8]) -> GramKey {
        let folded: Vec<u8> = bytes.iter().map(u8::to_ascii_lowercase).collect();
        GramKey::new(HashKey::UNKEYED.folded().hash_bytes(&folded))
    }

    #[test]
    fn literal_scan_has_content_spans() {
        let mut grams = Vec::new();
        let table = table();
        literal_scanner(&table).push_bytes(b"abcdef", 6, &mut |gram| {
            grams.push(gram.content_span());
        });

        assert!(!grams.is_empty());
        assert!(grams.iter().all(|span| span.start <= span.end));
        assert!(grams.iter().all(|span| span.end <= 6));
    }

    #[test]
    fn literal_scan_emits_raw_keys() {
        let literal = b"abcdef";
        let mut keys = HashSet::new();
        let table = table();
        literal_scanner(&table).push_bytes(literal, literal.len(), &mut |gram| {
            keys.insert(gram.key);
        });

        assert!(!keys.is_empty());
    }
}
