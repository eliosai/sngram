//! Monotonic-stack scanner over one gram key space.

use std::ops::Range;

use sngram_types::{ByteRange, GramKey, HashKey, ScannedGram, WeightTable};

use super::settings::ScanSettings;

/// Byte mapping applied to content before weighing and hashing.
#[derive(Debug, Clone, Copy)]
pub enum Transform {
    Raw,
    Folded,
}

/// Mapping from window offsets to public content spans.
#[derive(Debug, Clone, Copy)]
pub enum SpanMap {
    Document,
    Literal,
}

/// Which hull grams the space emits.
#[derive(Debug, Clone, Copy)]
pub enum EmitPolicy {
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

/// Streaming hull scanner for one key space and transform.
pub struct SpaceScanner<'t> {
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
    pub fn new(
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

    pub fn push_bytes<F>(&mut self, chunk: &[u8], content_bytes: usize, emit: &mut F)
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
