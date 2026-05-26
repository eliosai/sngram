//! Sparse n-gram extraction via monotonic stack (convex hull).

use sngram_types::{IndexGram, IndexGrams, QueryGram, QueryGrams, WeightTable};

const MIN_LEN: usize = 3;
const MAX_LEN: usize = 100;
const STACK_CAP: usize = 128;

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
    if len >= MIN_LEN && len <= MAX_LEN {
        emit(start, end);
    }
}

struct FixedStack {
    buf: [(usize, u32); STACK_CAP],
    len: usize,
}

impl FixedStack {
    #[inline]
    fn new() -> Self {
        Self { buf: [(0, 0); STACK_CAP], len: 0 }
    }

    #[inline]
    fn push(&mut self, pos: usize, weight: u32) {
        if self.len < STACK_CAP {
            self.buf[self.len] = (pos, weight);
            self.len += 1;
        }
    }

    #[inline]
    fn pop(&mut self) {
        self.len = self.len.saturating_sub(1);
    }

    #[inline]
    fn peek(&self) -> Option<(usize, u32)> {
        if self.len > 0 { Some(self.buf[self.len - 1]) } else { None }
    }

    #[inline]
    fn dedup(&mut self, w: u32) {
        while self.len > 0 && self.buf[self.len - 1].1 == w {
            self.len -= 1;
        }
    }
}

fn emit_covering(table: &WeightTable, literal: &[u8]) -> Vec<QueryGram> {
    if literal.len() < MIN_LEN {
        return Vec::new();
    }
    split_at_maxima(table, literal)
}

#[allow(clippy::indexing_slicing, reason = "indices bounded by loop range")]
fn split_at_maxima(table: &WeightTable, content: &[u8]) -> Vec<QueryGram> {
    let last = content.len() - 2;
    let mut grams = Vec::new();
    let mut start = 0;

    for i in 0..=last {
        if !is_local_max(table, content, i, last) {
            continue;
        }
        let end = (i + 2).min(content.len());
        if end - start >= MIN_LEN {
            grams.push(QueryGram::new(content[start..end].to_vec(), start));
        }
        start = i;
    }

    if content.len() - start >= MIN_LEN {
        grams.push(QueryGram::new(content[start..].to_vec(), start));
    }

    grams
}

#[inline]
#[allow(clippy::indexing_slicing, reason = "bounds checked by caller")]
fn is_local_max(table: &WeightTable, c: &[u8], i: usize, last: usize) -> bool {
    let w = table.weight(c[i], c[i + 1]);
    let left_ok = i == 0 || w > table.weight(c[i - 1], c[i]);
    let right_ok = i == last || w > table.weight(c[i + 1], c[i + 2]);
    left_ok && right_ok
}
