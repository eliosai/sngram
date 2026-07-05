//! Streaming sparse n-gram scanner.

use std::ops::Range;

use sngram_types::{ByteRange, GramKey, HashKey, ScanEvent, ScannedGram, WeightTable};

use super::{ScanSettings, facts::SummaryBuilder};

#[derive(Debug, Clone, Copy)]
enum Transform {
    Raw,
    Folded,
}

#[derive(Debug, Clone, Copy)]
enum SpanMap {
    Document,
    Literal,
}

pub struct DocumentScanner<'t> {
    primary: SpaceScanner<'t>,
    folded: SpaceScanner<'t>,
    summary: SummaryBuilder,
    content_bytes: usize,
    gram_count: u32,
}

impl<'t> DocumentScanner<'t> {
    pub fn new(table: &'t WeightTable) -> Self {
        Self {
            primary: SpaceScanner::new(
                table,
                HashKey::UNKEYED,
                Transform::Raw,
                SpanMap::Document,
                EmitPolicy::All,
            ),
            folded: SpaceScanner::new(
                table,
                HashKey::UNKEYED.folded(),
                Transform::Folded,
                SpanMap::Document,
                EmitPolicy::ChangedOnly,
            ),
            summary: SummaryBuilder::default(),
            content_bytes: 0,
            gram_count: 0,
        }
    }

    pub fn begin_document(&mut self, emit: &mut impl for<'event> FnMut(ScanEvent<'event>)) {
        self.push_sentinel(emit);
    }

    pub fn push_content(
        &mut self,
        chunk: &[u8],
        emit: &mut impl for<'event> FnMut(ScanEvent<'event>),
    ) {
        if chunk.is_empty() {
            return;
        }

        self.summary.observe(chunk);
        self.content_bytes += chunk.len();
        self.push_to_spaces(chunk, emit);
    }

    pub fn finish_document(&mut self, emit: &mut impl for<'event> FnMut(ScanEvent<'event>)) {
        self.push_sentinel(emit);
        let summary = self.summary.finish(self.gram_count);
        emit(ScanEvent::Finish(&summary));
    }

    fn push_sentinel(&mut self, emit: &mut impl for<'event> FnMut(ScanEvent<'event>)) {
        self.push_to_spaces(&[ScanSettings::DOCUMENT_SENTINEL], emit);
    }

    fn push_to_spaces(
        &mut self,
        chunk: &[u8],
        emit: &mut impl for<'event> FnMut(ScanEvent<'event>),
    ) {
        let content_bytes = self.content_bytes;
        let gram_count = &mut self.gram_count;
        self.primary.push_bytes(chunk, content_bytes, &mut |gram| {
            *gram_count = gram_count.saturating_add(1);
            emit(ScanEvent::Gram(gram));
        });

        self.folded.push_bytes(chunk, content_bytes, &mut |gram| {
            *gram_count = gram_count.saturating_add(1);
            emit(ScanEvent::Gram(gram));
        });
    }
}

pub fn scan_literal(table: &WeightTable, literal: &[u8], mut emit: impl FnMut(ScannedGram)) {
    let mut scanner = SpaceScanner::new(
        table,
        HashKey::UNKEYED,
        Transform::Raw,
        SpanMap::Literal,
        EmitPolicy::All,
    );
    scanner.push_bytes(literal, literal.len(), &mut emit);
}

#[derive(Debug, Clone, Copy)]
enum EmitPolicy {
    All,
    ChangedOnly,
}

struct SpaceScanner<'t> {
    matrix: &'t [u32; 65_536],
    window: [u8; ScanSettings::WINDOW_CAP],
    changed: [bool; ScanSettings::WINDOW_CAP],
    window_len: usize,
    base: usize,
    stack: [(usize, u32); ScanSettings::STACK_CAP],
    stack_len: usize,
    prefix_hash: u64,
    ring: [u64; ScanSettings::PREFIX_RING],
    key: HashKey,
    transform: Transform,
    span_map: SpanMap,
    emit_policy: EmitPolicy,
}

impl<'t> SpaceScanner<'t> {
    fn new(
        table: &'t WeightTable,
        key: HashKey,
        transform: Transform,
        span_map: SpanMap,
        emit_policy: EmitPolicy,
    ) -> Self {
        Self {
            matrix: table.matrix(),
            window: [0; ScanSettings::WINDOW_CAP],
            changed: [false; ScanSettings::WINDOW_CAP],
            window_len: 0,
            base: 0,
            stack: [(0, 0); ScanSettings::STACK_CAP],
            stack_len: 0,
            prefix_hash: 0,
            ring: [0; ScanSettings::PREFIX_RING],
            key,
            transform,
            span_map,
            emit_policy,
        }
    }

    #[allow(
        clippy::excessive_nesting,
        clippy::too_many_lines,
        reason = "the hot loop is kept as one linear automaton; splitting it costs measured throughput"
    )]
    fn push_bytes<F>(&mut self, chunk: &[u8], content_bytes: usize, emit: &mut F)
    where
        F: FnMut(ScannedGram),
    {
        let (mut top_start, mut top_weight) = if self.stack_len > 0 {
            self.stack[self.stack_len - 1]
        } else {
            (0, 0)
        };
        let mut prefix_hash = self.prefix_hash;
        let mut rest = chunk;
        while !rest.is_empty() {
            if self.window_len == ScanSettings::WINDOW_CAP {
                self.compact();
            }

            let take = rest.len().min(ScanSettings::WINDOW_CAP - self.window_len);
            let filled = self.window_len;
            self.copy_chunk(filled, &rest[..take]);
            self.window_len += take;
            rest = &rest[take..];

            if filled == 0 && take > 0 {
                prefix_hash = u64::from(self.window[0]);
                self.ring[0] = prefix_hash;
            }

            for window_idx in filled.max(1)..filled + take {
                prefix_hash = self
                    .key
                    .advance_prefix_hash(prefix_hash, self.window[window_idx]);
                self.ring[(self.base + window_idx) & ScanSettings::PREFIX_RING_MASK] = prefix_hash;
                let weight = self.matrix[(usize::from(self.window[window_idx - 1]) << 8)
                    | usize::from(self.window[window_idx])];
                let gram_start = self.base + window_idx - 1;
                let gram_end = self.base + window_idx + 1;
                while self.stack_len > 0 {
                    if top_weight >= weight {
                        self.emit_window(prefix_hash, top_start..gram_end, content_bytes, emit);
                        if top_weight == weight {
                            self.stack_len -= 1;
                        }
                        break;
                    }
                    self.stack_len -= 1;
                    self.emit_window(prefix_hash, top_start..gram_end, content_bytes, emit);
                    if self.stack_len == 0 {
                        break;
                    }
                    (top_start, top_weight) = self.stack[self.stack_len - 1];
                }
                if self.stack_len >= ScanSettings::STACK_CAP {
                    self.stack.copy_within(1.., 0);
                    self.stack_len -= 1;
                }
                self.stack[self.stack_len] = (gram_start, weight);
                self.stack_len += 1;
                top_start = gram_start;
                top_weight = weight;
            }
        }
        self.prefix_hash = prefix_hash;
    }

    fn copy_chunk(&mut self, filled: usize, chunk: &[u8]) {
        match self.transform {
            Transform::Raw => {
                self.window[filled..filled + chunk.len()].copy_from_slice(chunk);
                self.changed[filled..filled + chunk.len()].fill(false);
            },
            Transform::Folded => {
                for ((dst, changed), src) in self.window[filled..filled + chunk.len()]
                    .iter_mut()
                    .zip(&mut self.changed[filled..filled + chunk.len()])
                    .zip(chunk)
                {
                    let folded = src.to_ascii_lowercase();
                    *dst = folded;
                    *changed = folded != *src;
                }
            },
        }
    }

    #[inline]
    fn emit_window<F>(
        &self,
        prefix_after_end: u64,
        span: Range<usize>,
        content_bytes: usize,
        emit: &mut F,
    ) where
        F: FnMut(ScannedGram),
    {
        let start = span.start;
        let end = span.end;
        let len = end - start;
        if !self.should_emit(start, end, len) {
            return;
        }

        let prefix_before_start = self.prefix_before(start);
        let span = self.map_span(start, end, content_bytes);
        emit(ScannedGram {
            key: GramKey(
                self.key
                    .hash_from_prefixes(prefix_after_end, prefix_before_start, len),
            ),
            span,
        });
    }

    fn should_emit(&self, start: usize, end: usize, len: usize) -> bool {
        ScanSettings::emits_len(len) && self.emit_policy_allows(start, end)
    }

    fn emit_policy_allows(&self, start: usize, end: usize) -> bool {
        !matches!(self.emit_policy, EmitPolicy::ChangedOnly)
            || self.changed[start - self.base..end - self.base]
                .iter()
                .any(|&changed| changed)
    }

    const fn prefix_before(&self, start: usize) -> u64 {
        if start == 0 {
            0
        } else {
            self.ring[(start - 1) & ScanSettings::PREFIX_RING_MASK]
        }
    }

    fn map_span(&self, start: usize, end: usize, content_bytes: usize) -> ByteRange {
        match self.span_map {
            SpanMap::Document => ByteRange::new(
                start.saturating_sub(1).min(content_bytes),
                end.saturating_sub(1).min(content_bytes),
            ),
            SpanMap::Literal => ByteRange::new(start.min(content_bytes), end.min(content_bytes)),
        }
    }

    fn compact(&mut self) {
        const DROP: usize = ScanSettings::WINDOW_CAP - ScanSettings::WINDOW_KEEP;
        self.window.copy_within(DROP.., 0);
        self.changed.copy_within(DROP.., 0);
        self.window_len = ScanSettings::WINDOW_KEEP;
        self.base += DROP;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::io::Cursor;

    use sngram_types::{ByteRange, GramKey, HashKey, ScanSummary, ScannedGram};

    use super::*;

    fn table() -> WeightTable {
        WeightTable::from_weight_fn(|c1, c2| crc32fast::hash(&[c1, c2]))
    }

    fn collect(doc: &[u8]) -> (Vec<ScannedGram>, ScanSummary) {
        let table = table();
        let mut grams = Vec::new();
        let mut summary = None;
        crate::scan::scan(&table, Cursor::new(doc), |event| match event {
            ScanEvent::Gram(gram) => grams.push(gram),
            ScanEvent::Finish(done) => summary = Some(*done),
        })
        .expect("scan succeeds");
        let summary = summary.unwrap_or_else(|| panic!("scanner emits summary"));
        (grams, summary)
    }

    #[test]
    fn emits_keys_and_summary_count() {
        let (grams, summary) = collect(b"AbcDef");

        assert!(!grams.is_empty());
        assert_eq!(usize::try_from(summary.gram_count).unwrap(), grams.len());
    }

    #[test]
    fn folded_supplement_is_skipped_for_lowercase_ascii() {
        let (lower, _) = collect(b"abcdef");
        let (upper, _) = collect(b"ABCDEF");

        assert!(
            upper.len() > lower.len(),
            "uppercase content should add folded supplement keys"
        );
    }

    #[test]
    fn folded_supplement_keys_hash_changed_folded_spans() {
        let doc = b"xxABCyyDEFzz";
        let (grams, _) = collect(doc);

        assert!(
            grams.iter().any(|gram| {
                let span = gram.span.as_range();
                span.start > 0
                    && span.end < doc.len()
                    && doc[span.clone()].iter().any(u8::is_ascii_uppercase)
                    && gram.key == folded_key(&doc[span])
            }),
            "uppercase content must emit folded supplement keys for changed interior spans",
        );
    }

    #[test]
    fn lowercase_ascii_does_not_emit_folded_supplement_keys() {
        let doc = b"xxabcyydefzz";
        let (grams, _) = collect(doc);

        assert!(
            grams.iter().all(|gram| {
                let span = gram.span.as_range();
                span.start == 0 || span.end == doc.len() || gram.key != folded_key(&doc[span])
            }),
            "unchanged lowercase spans must not be duplicated into the folded key space",
        );
    }

    #[test]
    fn boundary_spans_can_hash_virtual_sentinels() {
        let (grams, _) = collect(b"A");
        let sentinel_key = GramKey(HashKey::UNKEYED.hash_bytes(b"\nA\n"));

        assert!(
            grams
                .iter()
                .any(|gram| gram.span == ByteRange::new(0, 1) && gram.key == sentinel_key),
            "the public content span is not always the byte range that was hashed",
        );
    }

    #[test]
    fn document_spans_stay_in_content_bounds() {
        let table = table();
        crate::scan::scan(&table, Cursor::new(b"abc"), |event| {
            if let ScanEvent::Gram(gram) = event {
                assert!(gram.span.start <= gram.span.end);
                assert!(gram.span.end <= 3);
            }
        })
        .expect("scan succeeds");
    }

    #[test]
    fn summary_reports_document_facts() {
        let (_, summary) = collect("Ab\r\né".as_bytes());

        assert_eq!(summary.byte_len, "Ab\r\né".len() as u64);
        assert_eq!(summary.line_count, 2);
        assert!(summary.flags.has_lf());
        assert!(summary.flags.has_crlf());
        assert!(summary.flags.has_ascii_upper());
        assert!(summary.flags.has_ascii_lower());
        assert!(summary.flags.has_non_ascii());
    }

    fn folded_key(bytes: &[u8]) -> GramKey {
        let folded: Vec<u8> = bytes.iter().map(u8::to_ascii_lowercase).collect();
        GramKey(HashKey::UNKEYED.folded().hash_bytes(&folded))
    }

    #[test]
    fn literal_scan_has_content_spans() {
        let mut grams = Vec::new();
        scan_literal(&table(), b"abcdef", |gram| {
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
        scan_literal(&table(), literal, |gram| {
            keys.insert(gram.key);
        });

        assert!(!keys.is_empty());
    }
}
