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

#[derive(Debug, Clone, Copy)]
struct FilledRange {
    start: usize,
    end: usize,
}

impl FilledRange {
    const fn is_empty(self) -> bool {
        self.start == self.end
    }

    fn scan_indices(self) -> core::ops::Range<usize> {
        self.start.max(1)..self.end
    }
}

#[derive(Debug, Clone, Copy)]
struct StackEntry {
    start: usize,
    weight: u32,
}

impl StackEntry {
    const ZERO: Self = Self {
        start: 0,
        weight: 0,
    };

    const fn new(start: usize, weight: u32) -> Self {
        Self { start, weight }
    }
}

#[derive(Debug, Clone, Copy)]
struct WeightedEdge {
    start: usize,
    end: usize,
    weight: u32,
    prefix_after_end: u64,
}

#[derive(Debug, Clone, Copy)]
struct ScanCursor {
    prefix_hash: u64,
    content_bytes: usize,
    top: StackEntry,
}

impl ScanCursor {
    const fn new(prefix_hash: u64, content_bytes: usize, top: StackEntry) -> Self {
        Self {
            prefix_hash,
            content_bytes,
            top,
        }
    }
}

struct SpaceScanner<'t> {
    matrix: &'t [u32; 65_536],
    window: [u8; ScanSettings::WINDOW_CAP],
    changed: [bool; ScanSettings::WINDOW_CAP],
    window_len: usize,
    base: usize,
    stack: [StackEntry; ScanSettings::STACK_CAP],
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
            stack: [StackEntry::ZERO; ScanSettings::STACK_CAP],
            stack_len: 0,
            prefix_hash: 0,
            ring: [0; ScanSettings::PREFIX_RING],
            key,
            transform,
            span_map,
            emit_policy,
        }
    }

    fn push_bytes<F>(&mut self, chunk: &[u8], content_bytes: usize, emit: &mut F)
    where
        F: FnMut(ScannedGram),
    {
        let mut cursor = ScanCursor::new(self.prefix_hash, content_bytes, self.stack_top());
        let mut rest = chunk;
        while !rest.is_empty() {
            self.compact_if_full();
            let range = self.append_window_chunk(&mut rest);
            self.seed_first_prefix(range, &mut cursor);
            self.scan_filled_range(range, &mut cursor, emit);
        }
        self.prefix_hash = cursor.prefix_hash;
    }

    fn compact_if_full(&mut self) {
        if self.window_len == ScanSettings::WINDOW_CAP {
            self.compact();
        }
    }

    fn append_window_chunk(&mut self, rest: &mut &[u8]) -> FilledRange {
        let take = rest.len().min(ScanSettings::WINDOW_CAP - self.window_len);
        let start = self.window_len;
        self.copy_chunk(start, &rest[..take]);
        self.window_len += take;
        *rest = &rest[take..];
        FilledRange {
            start,
            end: start + take,
        }
    }

    fn seed_first_prefix(&mut self, range: FilledRange, cursor: &mut ScanCursor) {
        if range.start != 0 || range.is_empty() {
            return;
        }
        cursor.prefix_hash = u64::from(self.window[0]);
        self.ring[0] = cursor.prefix_hash;
    }

    fn scan_filled_range<F>(&mut self, range: FilledRange, cursor: &mut ScanCursor, emit: &mut F)
    where
        F: FnMut(ScannedGram),
    {
        for window_idx in range.scan_indices() {
            let edge = self.weighted_edge(window_idx, cursor.prefix_hash);
            cursor.prefix_hash = edge.prefix_after_end;
            self.close_stack_for(edge, cursor.content_bytes, emit, &mut cursor.top);
            self.push_stack(edge);
            cursor.top = StackEntry::new(edge.start, edge.weight);
        }
    }

    fn weighted_edge(&mut self, window_idx: usize, prefix_before_end: u64) -> WeightedEdge {
        let prefix_after_end = self
            .key
            .advance_prefix_hash(prefix_before_end, self.window[window_idx]);
        self.ring[(self.base + window_idx) & ScanSettings::PREFIX_RING_MASK] = prefix_after_end;
        WeightedEdge {
            start: self.base + window_idx - 1,
            end: self.base + window_idx + 1,
            weight: self.weight_at(window_idx),
            prefix_after_end,
        }
    }

    fn weight_at(&self, window_idx: usize) -> u32 {
        self.matrix
            [(usize::from(self.window[window_idx - 1]) << 8) | usize::from(self.window[window_idx])]
    }

    fn close_stack_for<F>(
        &mut self,
        edge: WeightedEdge,
        content_bytes: usize,
        emit: &mut F,
        top: &mut StackEntry,
    ) where
        F: FnMut(ScannedGram),
    {
        while self.stack_len > 0 {
            self.emit_stack_entry(*top, edge, content_bytes, emit);
            if top.weight >= edge.weight {
                self.pop_equal_weight(top.weight, edge.weight);
                return;
            }
            self.stack_len -= 1;
            let Some(next) = self.stack_top_entry() else {
                return;
            };
            *top = next;
        }
    }

    fn emit_stack_entry<F>(
        &self,
        entry: StackEntry,
        edge: WeightedEdge,
        content_bytes: usize,
        emit: &mut F,
    ) where
        F: FnMut(ScannedGram),
    {
        self.emit_window(
            edge.prefix_after_end,
            entry.start..edge.end,
            content_bytes,
            emit,
        );
    }

    const fn pop_equal_weight(&mut self, top_weight: u32, edge_weight: u32) {
        if top_weight == edge_weight {
            self.stack_len -= 1;
        }
    }

    fn push_stack(&mut self, edge: WeightedEdge) {
        self.make_stack_room();
        self.stack[self.stack_len] = StackEntry::new(edge.start, edge.weight);
        self.stack_len += 1;
    }

    fn make_stack_room(&mut self) {
        if self.stack_len == ScanSettings::STACK_CAP {
            self.stack.copy_within(1.., 0);
            self.stack_len -= 1;
        }
    }

    fn stack_top(&self) -> StackEntry {
        self.stack_top_entry()
            .unwrap_or_else(|| StackEntry::new(0, 0))
    }

    fn stack_top_entry(&self) -> Option<StackEntry> {
        self.stack.get(self.stack_len.checked_sub(1)?).copied()
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
            key: self.gram_key(prefix_after_end, prefix_before_start, len),
            span,
        });
    }

    const fn gram_key(
        &self,
        prefix_after_end: u64,
        prefix_before_start: u64,
        len: usize,
    ) -> GramKey {
        GramKey(
            self.key
                .hash_from_prefixes(prefix_after_end, prefix_before_start, len),
        )
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

    fn collect(doc: &[u8]) -> CollectedScan {
        let table = table();
        let mut grams = Vec::new();
        let mut summary = None;
        crate::scan::scan(&table, Cursor::new(doc), |event| match event {
            ScanEvent::Gram(gram) => grams.push(gram),
            ScanEvent::Finish(done) => summary = Some(*done),
        })
        .expect("scan succeeds");
        let summary = summary.unwrap_or_else(|| panic!("scanner emits summary"));
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
        let sentinel_key = GramKey(HashKey::UNKEYED.hash_bytes(b"\nA\n"));

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
    fn scan_grams_do_not_depend_on_reader_chunk_size() {
        let doc = repeated_source(9000);
        let expected = canonical_grams(collect(&doc).grams);

        for cap in [1, 2, 127, 128, 129, 895, 896, 1024] {
            let table = table();
            let reader = std::io::BufReader::with_capacity(cap, Cursor::new(&doc));
            let mut grams = Vec::new();
            crate::scan::scan(&table, reader, |event| {
                collect_gram(event, &mut grams);
            })
            .expect("scan succeeds");
            assert_eq!(canonical_grams(grams), expected, "chunk capacity {cap}");
        }
    }

    fn collect_gram(event: ScanEvent<'_>, grams: &mut Vec<ScannedGram>) {
        let ScanEvent::Gram(gram) = event else {
            return;
        };
        grams.push(gram);
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
