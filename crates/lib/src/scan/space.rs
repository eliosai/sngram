//! Monotonic-stack scanner over one gram key space.

use sngram_types::{ByteRange, GramKey, HashKey, ScannedGram, WeightTable};

use super::settings::ScanSettings;

/// Byte mapping applied to content before weighing and hashing.
#[derive(Debug, Clone, Copy)]
pub enum Transform {
    Raw,
    Folded,
}

/// Mapping from stream offsets to public content spans.
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
struct ScanCursor {
    pos: usize,
    prev: u8,
    prefix: u64,
    content_bytes: usize,
    top: StackEntry,
}

#[derive(Debug, Clone, Copy)]
struct Edge {
    end: usize,
    prefix: u64,
    content_bytes: usize,
}

/// Streaming hull scanner for one key space and transform.
pub struct SpaceScanner<'t> {
    matrix: &'t [u32; 65_536],
    stack: [StackEntry; ScanSettings::STACK_CAP],
    stack_len: usize,
    ring: [u64; ScanSettings::PREFIX_RING],
    prefix_hash: u64,
    pos: usize,
    prev: u8,
    last_changed_end: usize,
    key: HashKey,
    transform: Transform,
    span_sub: usize,
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
            stack: [StackEntry::ZERO; ScanSettings::STACK_CAP],
            stack_len: 0,
            ring: [0; ScanSettings::PREFIX_RING],
            prefix_hash: 0,
            pos: 0,
            prev: 0,
            last_changed_end: match emit_policy {
                EmitPolicy::All => usize::MAX,
                EmitPolicy::ChangedOnly => 0,
            },
            key,
            transform,
            span_sub: match span_map {
                SpanMap::Document => 1,
                SpanMap::Literal => 0,
            },
        }
    }

    /// Copy the rolling scan state from a space that saw identical bytes
    pub const fn mirror_from(&mut self, source: &Self) {
        self.stack = source.stack;
        self.stack_len = source.stack_len;
        self.ring = source.ring;
        self.prefix_hash = source.prefix_hash;
        self.pos = source.pos;
        self.prev = source.prev;
    }

    pub fn push_bytes<F>(&mut self, chunk: &[u8], content_bytes: usize, emit: &mut F)
    where
        F: FnMut(ScannedGram),
    {
        match self.transform {
            Transform::Raw => self.run_chunk(chunk, content_bytes, emit, |byte| byte),
            Transform::Folded => {
                self.run_chunk(chunk, content_bytes, emit, |byte| byte.to_ascii_lowercase());
            },
        }
    }

    fn run_chunk<F, M>(&mut self, chunk: &[u8], content_bytes: usize, emit: &mut F, map: M)
    where
        F: FnMut(ScannedGram),
        M: Fn(u8) -> u8,
    {
        let mut rest = chunk;
        if self.pos == 0
            && let Some((&first, tail)) = rest.split_first()
        {
            self.seed_first_byte(first, map(first));
            rest = tail;
        }
        let mut cursor = ScanCursor {
            pos: self.pos,
            prev: self.prev,
            prefix: self.prefix_hash,
            content_bytes,
            top: self.stack[self.stack_len.saturating_sub(1)],
        };
        for &raw in rest {
            let byte = map(raw);
            self.step_byte(&mut cursor, byte, byte != raw, emit);
        }
        self.pos = cursor.pos;
        self.prev = cursor.prev;
        self.prefix_hash = cursor.prefix;
    }

    fn seed_first_byte(&mut self, raw: u8, byte: u8) {
        self.prefix_hash = u64::from(byte);
        self.ring[0] = self.prefix_hash;
        if byte != raw {
            self.last_changed_end = 1;
        }
        self.prev = byte;
        self.pos = 1;
    }

    #[inline]
    fn step_byte<F>(&mut self, cursor: &mut ScanCursor, byte: u8, changed: bool, emit: &mut F)
    where
        F: FnMut(ScannedGram),
    {
        let pos = cursor.pos;
        let weight = self.matrix[(usize::from(cursor.prev) << 8) | usize::from(byte)];
        let prefix = self.key.advance_prefix_hash(cursor.prefix, byte);
        cursor.prefix = prefix;
        self.ring[pos & ScanSettings::PREFIX_RING_MASK] = prefix;
        if changed {
            self.last_changed_end = pos + 1;
        }
        let edge = Edge {
            end: pos + 1,
            prefix,
            content_bytes: cursor.content_bytes,
        };
        self.close_stack(cursor, weight, edge, emit);
        self.push_entry(StackEntry::new(pos - 1, weight));
        cursor.top = StackEntry::new(pos - 1, weight);
        cursor.prev = byte;
        cursor.pos = pos + 1;
    }

    #[inline]
    fn close_stack<F>(&mut self, cursor: &mut ScanCursor, weight: u32, edge: Edge, emit: &mut F)
    where
        F: FnMut(ScannedGram),
    {
        while self.stack_len > 0 {
            let top = cursor.top;
            self.emit_hull(top.start, edge, emit);
            if top.weight >= weight {
                self.stack_len -= usize::from(top.weight == weight);
                return;
            }
            self.stack_len -= 1;
            let Some(next) = self.stack_len.checked_sub(1) else {
                return;
            };
            cursor.top = self.stack[next];
        }
    }

    // hull entries start at least two bytes back, so len >= 3 always holds
    #[inline]
    fn emit_hull<F>(&self, start: usize, edge: Edge, emit: &mut F)
    where
        F: FnMut(ScannedGram),
    {
        let len = edge.end - start;
        if len > ScanSettings::MAX_GRAM_LEN || self.last_changed_end <= start {
            return;
        }
        // ring slot 127 stays zero while a start-0 gram can still pass the length filter
        let before = self.ring[start.wrapping_sub(1) & ScanSettings::PREFIX_RING_MASK];
        emit(ScannedGram {
            key: GramKey(self.key.hash_from_prefixes(edge.prefix, before, len)),
            span: ByteRange::new(
                start.saturating_sub(self.span_sub).min(edge.content_bytes),
                edge.end
                    .saturating_sub(self.span_sub)
                    .min(edge.content_bytes),
            ),
        });
    }

    fn push_entry(&mut self, entry: StackEntry) {
        if self.stack_len == ScanSettings::STACK_CAP {
            self.stack.copy_within(1.., 0);
            self.stack_len -= 1;
        }
        self.stack[self.stack_len] = entry;
        self.stack_len += 1;
    }
}
