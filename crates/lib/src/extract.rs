//! Sparse n-gram extraction via monotonic stack (convex hull).

use std::collections::VecDeque;

use sngram_types::{IndexGram, IndexGrams, WeightTable};

/// Shortest gram emitted or matched; a sparse gram spans at least one bigram.
pub const MIN_LEN: usize = 3;
/// Longest gram emitted; bounds index entries and covering-set members.
pub const MAX_LEN: usize = 100;
const STACK_CAP: usize = 128;

/// streaming window: keeps recent bytes so an emitted gram stays contiguous, larger than the kept tail to amortize compaction
const WINDOW_CAP: usize = 256;
/// bytes kept on compaction, at least the longest gram so every still-emittable gram start stays in the window
const WINDOW_KEEP: usize = 128;

#[allow(
    clippy::indexing_slicing,
    reason = "scan emits start..end within content"
)]
pub fn all<'a>(table: &WeightTable, content: &'a [u8]) -> IndexGrams<'a> {
    if content.len() < MIN_LEN {
        return IndexGrams::new(Vec::new());
    }
    let n = content.len() - 1;
    let mut grams = Vec::with_capacity(n * 2);
    scan(table, content, |start, end| {
        grams.push(IndexGram::new(&content[start..end], start));
    });
    IndexGrams::new(grams)
}

/// Zero-allocation extraction. Calls `emit` for each gram's (start, end).
///
/// Use this instead of [`all`] when you don't need to collect grams —
/// e.g. when hashing and inserting directly into an inverted index.
#[inline]
#[allow(clippy::indexing_slicing, reason = "bounds enforced by pair_count")]
pub fn scan(table: &WeightTable, content: &[u8], mut emit: impl FnMut(usize, usize)) {
    if content.len() < MIN_LEN {
        return;
    }
    let n = content.len() - 1;
    let mut stack = FixedStack::new();

    for i in 0..n {
        let w = table.weight(content[i], content[i + 1]);
        let end = i + 2;
        drain_emit(&mut stack, end, w, &mut emit);
        top_emit(&stack, end, &mut emit);
        stack.dedup(w);
        stack.push(i, w);
    }
}

/// streaming sparse n-gram extraction that holds a bounded window, never the whole document
pub struct StreamScanner<'t> {
    table: &'t WeightTable,
    window: [u8; WINDOW_CAP],
    wlen: usize,
    base: usize,
    stack: FixedStack,
}

impl<'t> StreamScanner<'t> {
    /// new scanner bound to a weight table, ready to receive byte chunks
    #[must_use]
    pub const fn new(table: &'t WeightTable) -> Self {
        Self {
            table,
            window: [0; WINDOW_CAP],
            wlen: 0,
            base: 0,
            stack: FixedStack::new(),
        }
    }

    /// feed the next chunk, emitting each gram's bytes as it closes, identical to scan over the concatenation of all chunks
    #[allow(
        clippy::indexing_slicing,
        reason = "wlen stays <= WINDOW_CAP and a valid gram start is within MAX_LEN of end, kept in the window by WINDOW_KEEP"
    )]
    pub fn push(&mut self, chunk: &[u8], mut emit: impl FnMut(&[u8])) {
        for &byte in chunk {
            if self.wlen == WINDOW_CAP {
                self.compact();
            }
            self.window[self.wlen] = byte;
            self.wlen += 1;
            if self.wlen < 2 {
                continue;
            }
            let weight = self
                .table
                .weight(self.window[self.wlen - 2], self.window[self.wlen - 1]);
            let pos = self.base + self.wlen - 2;
            let end = self.base + self.wlen;
            let base = self.base;
            let window = &self.window;
            let stack = &mut self.stack;
            let mut sink = |start: usize, finish: usize| emit(&window[start - base..finish - base]);
            drain_emit(stack, end, weight, &mut sink);
            top_emit(stack, end, &mut sink);
            stack.dedup(weight);
            stack.push(pos, weight);
        }
    }

    /// end the current document and reset for the next, emitting nothing since scan leaves no closed grams at end of input
    pub const fn finish(&mut self) {
        self.wlen = 0;
        self.base = 0;
        self.stack = FixedStack::new();
    }

    /// slide the still-emittable tail to the window front so more bytes fit, dropping only bytes too old to start a gram
    fn compact(&mut self) {
        const DROP: usize = WINDOW_CAP - WINDOW_KEEP;
        self.window.copy_within(DROP.., 0);
        self.wlen = WINDOW_KEEP;
        self.base += DROP;
    }
}

/// drive a scanner from an async buffered reader, reusing its buffer so nothing is allocated for reads
#[cfg(feature = "stream")]
impl StreamScanner<'_> {
    /// stream a whole reader through the scanner, emitting each gram, returns a forwarded io error if the read fails
    #[allow(
        clippy::missing_errors_doc,
        reason = "the only failure is a forwarded reader io error, named in the summary"
    )]
    pub async fn index_reader<R>(
        &mut self,
        mut reader: R,
        mut emit: impl FnMut(&[u8]),
    ) -> std::io::Result<()>
    where
        R: tokio::io::AsyncBufRead + Unpin,
    {
        use tokio::io::AsyncBufReadExt;
        loop {
            let chunk = reader.fill_buf().await?;
            if chunk.is_empty() {
                break;
            }
            let len = chunk.len();
            self.push(chunk, &mut emit);
            reader.consume(len);
        }
        self.finish();
        Ok(())
    }
}

/// Covering grams of a single literal, as raw bytes. The query analysis ANDs
/// these per literal: a document containing `literal` contains all of them
/// (`cover(L) ⊆ scan(D)` for any `D ⊇ L`), so none is a false negative.
#[allow(clippy::indexing_slicing, reason = "cover emits start..end within literal")]
#[must_use]
pub fn cover_one(table: &WeightTable, literal: &[u8]) -> Vec<Vec<u8>> {
    let mut grams = Vec::new();
    cover(table, literal, |start, end| {
        if (MIN_LEN..=MAX_LEN).contains(&(end - start)) {
            grams.push(literal[start..end].to_vec());
        }
    });
    grams
}

#[inline]
fn drain_emit(stack: &mut FixedStack, end: usize, w: u32, emit: &mut impl FnMut(usize, usize)) {
    while let Some((start, sw)) = stack.peek() {
        if sw >= w {
            break;
        }
        stack.pop();
        try_emit(start, end, emit);
    }
}

#[inline]
fn top_emit(stack: &FixedStack, end: usize, emit: &mut impl FnMut(usize, usize)) {
    if let Some((start, _)) = stack.peek() {
        try_emit(start, end, emit);
    }
}

#[inline]
fn try_emit(start: usize, end: usize, emit: &mut impl FnMut(usize, usize)) {
    let len = end - start;
    if (MIN_LEN..=MAX_LEN).contains(&len) {
        emit(start, end);
    }
}

struct FixedStack {
    buf: [(usize, u32); STACK_CAP],
    len: usize,
}

#[allow(clippy::indexing_slicing, reason = "indices stay < len <= STACK_CAP")]
impl FixedStack {
    #[inline]
    const fn new() -> Self {
        Self {
            buf: [(0, 0); STACK_CAP],
            len: 0,
        }
    }

    #[inline]
    fn push(&mut self, pos: usize, weight: u32) {
        if self.len == STACK_CAP {
            self.evict_oldest();
        }
        self.buf[self.len] = (pos, weight);
        self.len += 1;
    }

    /// drop the oldest entry when full, it is too far back to start a gram within the length limit
    #[cold]
    #[inline(never)]
    fn evict_oldest(&mut self) {
        self.buf.copy_within(1.., 0);
        self.len -= 1;
    }

    #[inline]
    const fn pop(&mut self) {
        self.len = self.len.saturating_sub(1);
    }

    #[inline]
    const fn peek(&self) -> Option<(usize, u32)> {
        if self.len > 0 {
            Some(self.buf[self.len - 1])
        } else {
            None
        }
    }

    #[inline]
    const fn dedup(&mut self, w: u32) {
        while self.len > 0 && self.buf[self.len - 1].1 == w {
            self.len -= 1;
        }
    }
}

/// Minimal covering n-grams (danlark1 `BuildCoveringNgrams`): the same hull as
/// [`scan`] restricted to the minimal set, so `cover(L)` is always a subset of
/// `scan(D)` for any `D` containing `L` — the guarantee against missed matches.
#[allow(
    clippy::indexing_slicing,
    reason = "front read only while deque non-empty"
)]
fn cover(table: &WeightTable, s: &[u8], mut emit: impl FnMut(usize, usize)) {
    let mut stack: VecDeque<(u32, usize)> = VecDeque::new();

    for i in 0..s.len().saturating_sub(1) {
        let w = table.weight(s[i], s[i + 1]);
        if stack.len() > 1 && i + 3 - stack[0].1 >= MAX_LEN {
            emit(stack[0].1, stack[1].1 + 2);
            stack.pop_front();
        }
        while let Some(&(top, pos)) = stack.back() {
            if w <= top {
                break;
            }
            if stack[0].0 == top {
                glue_plateau(&mut stack, pos, i + 2, &mut emit);
            }
            stack.pop_back();
        }
        stack.push_back((w, i));
    }
    drain(&mut stack, &mut emit);
}

/// Emit the consecutive grams of an equal-weight plateau, left to right.
fn glue_plateau(
    stack: &mut VecDeque<(u32, usize)>,
    back_pos: usize,
    end: usize,
    emit: &mut impl FnMut(usize, usize),
) {
    emit(back_pos, end);
    drain(stack, emit);
}

/// Pop the stack down to one entry, emitting the gram spanning each popped pair.
fn drain(stack: &mut VecDeque<(u32, usize)>, emit: &mut impl FnMut(usize, usize)) {
    while stack.len() > 1 {
        let Some((_, top)) = stack.pop_back() else {
            break;
        };
        if let Some(&(_, below)) = stack.back() {
            emit(below, top + 2);
        }
    }
}
