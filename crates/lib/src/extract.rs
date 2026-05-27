//! Sparse n-gram extraction via monotonic stack (convex hull).

use std::collections::VecDeque;

use sngram_types::{IndexGram, IndexGrams, QueryGram, QueryGrams, WeightTable};

const MIN_LEN: usize = 3;
const MAX_LEN: usize = 100;
const STACK_CAP: usize = 128;

#[allow(clippy::indexing_slicing, reason = "scan emits start..end within content")]
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

pub fn covering(table: &WeightTable, literals: &[Vec<u8>]) -> QueryGrams {
    let grams = literals
        .iter()
        .filter(|lit| lit.len() >= MIN_LEN)
        .flat_map(|lit| emit_covering(table, lit))
        .collect();
    QueryGrams::new(grams)
}

#[inline]
fn drain_emit(
    stack: &mut FixedStack,
    end: usize,
    w: u32,
    emit: &mut impl FnMut(usize, usize),
) {
    while let Some((start, sw)) = stack.peek() {
        if sw >= w { break; }
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
        Self { buf: [(0, 0); STACK_CAP], len: 0 }
    }

    #[inline]
    fn push(&mut self, pos: usize, weight: u32) {
        // Full: oldest entry is too far back to yield a gram <= MAX_LEN; drop it.
        if self.len == STACK_CAP {
            self.buf.copy_within(1.., 0);
            self.len -= 1;
        }
        self.buf[self.len] = (pos, weight);
        self.len += 1;
    }

    #[inline]
    const fn pop(&mut self) {
        self.len = self.len.saturating_sub(1);
    }

    #[inline]
    const fn peek(&self) -> Option<(usize, u32)> {
        if self.len > 0 { Some(self.buf[self.len - 1]) } else { None }
    }

    #[inline]
    const fn dedup(&mut self, w: u32) {
        while self.len > 0 && self.buf[self.len - 1].1 == w {
            self.len -= 1;
        }
    }
}

#[allow(clippy::indexing_slicing, reason = "cover emits start..end within literal")]
fn emit_covering(table: &WeightTable, literal: &[u8]) -> Vec<QueryGram> {
    let mut grams = Vec::new();
    cover(table, literal, |start, end| {
        if (MIN_LEN..=MAX_LEN).contains(&(end - start)) {
            grams.push(QueryGram::new(literal[start..end].to_vec(), start));
        }
    });
    grams
}

/// Minimal covering n-grams (danlark1 `BuildCoveringNgrams`): the same hull as
/// [`scan`] restricted to the minimal set, so `cover(L)` is always a subset of
/// `scan(D)` for any `D` containing `L` — the guarantee against missed matches.
#[allow(clippy::indexing_slicing, reason = "front read only while deque non-empty")]
fn cover(table: &WeightTable, s: &[u8], mut emit: impl FnMut(usize, usize)) {
    let mut stack: VecDeque<(u32, usize)> = VecDeque::new();

    for i in 0..s.len().saturating_sub(1) {
        let w = table.weight(s[i], s[i + 1]);
        if stack.len() > 1 && i + 3 - stack[0].1 >= MAX_LEN {
            emit(stack[0].1, stack[1].1 + 2);
            stack.pop_front();
        }
        while let Some(&(top, pos)) = stack.back() {
            if w <= top { break; }
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
        let Some((_, top)) = stack.pop_back() else { break; };
        if let Some(&(_, below)) = stack.back() {
            emit(below, top + 2);
        }
    }
}
