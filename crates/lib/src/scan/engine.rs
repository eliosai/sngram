//! Streaming sparse n-gram scanner.

use std::ops::Range;

use sngram_types::{
    Boundary, GramSpace, HashKey, ScanEvent, ScanSummary, ScannedGram, WeightTable,
};

use crate::scan::{facts::FactBuilder, settings::ScanSettings};

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
    facts: FactBuilder,
    content_bytes: usize,
    primary_grams: usize,
    folded_grams: usize,
}

impl<'t> DocumentScanner<'t> {
    pub fn new(table: &'t WeightTable) -> Self {
        Self {
            primary: SpaceScanner::new(
                table,
                GramSpace::Primary,
                Transform::Raw,
                SpanMap::Document,
            ),
            folded: SpaceScanner::new(
                table,
                GramSpace::Folded,
                Transform::Folded,
                SpanMap::Document,
            ),
            facts: FactBuilder::default(),
            content_bytes: 0,
            primary_grams: 0,
            folded_grams: 0,
        }
    }

    pub fn begin_document<F>(&mut self, emit: &mut F)
    where
        F: for<'a> FnMut(ScanEvent<'a>),
    {
        self.push_sentinel(emit);
    }

    pub fn push_content<F>(&mut self, chunk: &[u8], emit: &mut F)
    where
        F: for<'a> FnMut(ScanEvent<'a>),
    {
        if chunk.is_empty() {
            return;
        }

        self.facts.observe(chunk);
        self.content_bytes += chunk.len();
        self.push_to_spaces(chunk, emit);
    }

    pub fn finish_document<F>(&mut self, emit: &mut F)
    where
        F: for<'a> FnMut(ScanEvent<'a>),
    {
        self.push_sentinel(emit);
        emit(ScanEvent::Finish(ScanSummary {
            content_bytes: self.content_bytes,
            scanned_bytes: self.content_bytes + ScanSettings::SENTINELS_PER_DOCUMENT,
            primary_grams: self.primary_grams,
            folded_grams: self.folded_grams,
            facts: self.facts.finish(),
        }));
    }

    fn push_sentinel<F>(&mut self, emit: &mut F)
    where
        F: for<'a> FnMut(ScanEvent<'a>),
    {
        self.push_to_spaces(&[ScanSettings::SENTINEL], emit);
    }

    fn push_to_spaces<F>(&mut self, chunk: &[u8], emit: &mut F)
    where
        F: for<'a> FnMut(ScanEvent<'a>),
    {
        let content_bytes = self.content_bytes;
        let primary_grams = &mut self.primary_grams;
        self.primary.push_bytes(chunk, content_bytes, &mut |gram| {
            *primary_grams += 1;
            emit(ScanEvent::Gram(gram));
        });

        let folded_grams = &mut self.folded_grams;
        self.folded.push_bytes(chunk, content_bytes, &mut |gram| {
            *folded_grams += 1;
            emit(ScanEvent::Gram(gram));
        });
    }
}

pub fn scan_literal(table: &WeightTable, literal: &[u8], mut emit: impl FnMut(ScannedGram<'_>)) {
    let mut scanner =
        SpaceScanner::new(table, GramSpace::Primary, Transform::Raw, SpanMap::Literal);
    scanner.push_bytes(literal, literal.len(), &mut emit);
}

struct SpaceScanner<'t> {
    matrix: &'t [u32; 65_536],
    window: [u8; ScanSettings::WINDOW_CAP],
    window_len: usize,
    base: usize,
    stack: [(usize, u32); ScanSettings::STACK_CAP],
    stack_len: usize,
    prefix_hash: u64,
    ring: [u64; ScanSettings::RING],
    key: HashKey,
    space: GramSpace,
    transform: Transform,
    span_map: SpanMap,
}

impl<'t> SpaceScanner<'t> {
    fn new(
        table: &'t WeightTable,
        space: GramSpace,
        transform: Transform,
        span_map: SpanMap,
    ) -> Self {
        Self {
            matrix: table.matrix(),
            window: [0; ScanSettings::WINDOW_CAP],
            window_len: 0,
            base: 0,
            stack: [(0, 0); ScanSettings::STACK_CAP],
            stack_len: 0,
            prefix_hash: 0,
            ring: [0; ScanSettings::RING],
            key: match space {
                GramSpace::Primary => HashKey::UNKEYED,
                GramSpace::Folded => HashKey::UNKEYED.folded(),
            },
            space,
            transform,
            span_map,
        }
    }

    #[allow(
        clippy::excessive_nesting,
        clippy::too_many_lines,
        reason = "the hot loop is kept as one linear automaton; splitting it costs measured throughput"
    )]
    fn push_bytes<F>(&mut self, chunk: &[u8], content_bytes: usize, emit: &mut F)
    where
        F: for<'a> FnMut(ScannedGram<'a>),
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
                self.ring[(self.base + window_idx) & ScanSettings::RING_MASK] = prefix_hash;
                let weight = self.matrix[(usize::from(self.window[window_idx - 1]) << 8)
                    | usize::from(self.window[window_idx])];
                let gram_start = self.base + window_idx - 1;
                let gram_end = self.base + window_idx + 1;
                while self.stack_len > 0 {
                    if top_weight >= weight {
                        self.emit_window(prefix_hash, top_start..gram_end, content_bytes, emit);
                        if top_weight == weight {
                            // Equal borders describe the same plateau edge; keep only the new top.
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
            Transform::Raw => self.window[filled..filled + chunk.len()].copy_from_slice(chunk),
            Transform::Folded => {
                for (dst, src) in self.window[filled..filled + chunk.len()]
                    .iter_mut()
                    .zip(chunk)
                {
                    *dst = src.to_ascii_lowercase();
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
        F: for<'a> FnMut(ScannedGram<'a>),
    {
        let start = span.start;
        let end = span.end;
        let len = end - start;
        if !(ScanSettings::MIN_LEN..=ScanSettings::MAX_LEN).contains(&len) {
            return;
        }

        let prefix_before_start = if start == 0 {
            0
        } else {
            self.ring[(start - 1) & ScanSettings::RING_MASK]
        };
        let (content_start, content_end, boundary) = self.map_span(start, end, content_bytes);
        emit(ScannedGram {
            bytes: &self.window[start - self.base..end - self.base],
            hash: self
                .key
                .hash_from_prefixes(prefix_after_end, prefix_before_start, len),
            space: self.space,
            scanned_start: start,
            scanned_end: end,
            content_start,
            content_end,
            boundary,
        });
    }

    fn map_span(&self, start: usize, end: usize, content_bytes: usize) -> (usize, usize, Boundary) {
        match self.span_map {
            SpanMap::Document => {
                let touches_start = start == 0;
                let touches_end = end > content_bytes + 1;
                (
                    start.saturating_sub(1).min(content_bytes),
                    end.saturating_sub(1).min(content_bytes),
                    Boundary::new(touches_start, touches_end),
                )
            },
            SpanMap::Literal => (
                start.min(content_bytes),
                end.min(content_bytes),
                Boundary::new(false, false),
            ),
        }
    }

    fn compact(&mut self) {
        const DROP: usize = ScanSettings::WINDOW_CAP - ScanSettings::WINDOW_KEEP;
        self.window.copy_within(DROP.., 0);
        self.window_len = ScanSettings::WINDOW_KEEP;
        self.base += DROP;
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    type CollectedGram = (GramSpace, Vec<u8>, u64, Boundary);

    fn table() -> WeightTable {
        WeightTable::from_weight_fn(|c1, c2| crc32fast::hash(&[c1, c2]))
    }

    fn collect(doc: &[u8]) -> (Vec<CollectedGram>, ScanSummary) {
        let table = table();
        let mut grams = Vec::new();
        let mut summary = None;
        crate::scan::scan(&table, Cursor::new(doc), |event| match event {
            ScanEvent::Gram(gram) => {
                grams.push((gram.space, gram.bytes.to_vec(), gram.hash, gram.boundary));
            },
            ScanEvent::Finish(done) => summary = Some(done),
        })
        .expect("scan succeeds");
        let summary = summary.unwrap_or_else(|| panic!("scanner emits summary"));
        (grams, summary)
    }

    #[test]
    fn emits_primary_and_folded_spaces() {
        let (grams, summary) = collect(b"AbcDef");

        assert!(
            grams
                .iter()
                .any(|(space, _, _, _)| *space == GramSpace::Primary)
        );
        assert!(
            grams
                .iter()
                .any(|(space, _, _, _)| *space == GramSpace::Folded)
        );
        assert_eq!(summary.grams(), grams.len());
    }

    #[test]
    fn folded_space_uses_folded_bytes_and_key() {
        let (grams, _) = collect(b"ABCdef");
        for (_, bytes, hash, _) in grams
            .iter()
            .filter(|(space, _, _, _)| *space == GramSpace::Folded)
        {
            assert_eq!(bytes, &bytes.to_ascii_lowercase());
            assert_eq!(*hash, HashKey::UNKEYED.folded().hash_bytes(bytes));
        }
    }

    #[test]
    fn document_boundaries_are_mapped_to_content_spans() {
        let table = table();
        let mut seen_leading = false;
        let mut seen_trailing = false;
        crate::scan::scan(&table, Cursor::new(b"abc"), |event| {
            if let ScanEvent::Gram(gram) = event {
                assert!(gram.content_start <= gram.content_end);
                assert!(gram.content_end <= 3);
                seen_leading |= gram.boundary.touches_start();
                seen_trailing |= gram.boundary.touches_end();
            }
        })
        .expect("scan succeeds");

        assert!(seen_leading);
        assert!(seen_trailing);
    }

    #[test]
    fn summary_reports_document_facts() {
        let (_, summary) = collect("Ab\r\né".as_bytes());

        assert_eq!(summary.content_bytes, "Ab\r\né".len());
        assert_eq!(
            summary.scanned_bytes,
            "Ab\r\né".len() + ScanSettings::SENTINELS_PER_DOCUMENT
        );
        assert!(summary.facts.has_lf());
        assert!(summary.facts.has_crlf());
        assert!(summary.facts.has_upper_ascii());
        assert!(summary.facts.has_lower_ascii());
        assert!(summary.facts.has_non_ascii());
    }

    #[test]
    fn literal_scan_has_no_virtual_boundaries() {
        let mut grams = Vec::new();
        scan_literal(&table(), b"abcdef", |gram| {
            grams.push((gram.content_span(), gram.boundary));
        });

        assert!(!grams.is_empty());
        assert!(grams.iter().all(|(_, boundary)| !boundary.touches_start()));
        assert!(grams.iter().all(|(_, boundary)| !boundary.touches_end()));
    }
}
