//! Literal covering grams for query planning.

use std::collections::VecDeque;

use sngram_types::{Gram, WeightTable};

use super::{ScanSettings, engine};

/// Minimal covering grams of a single literal.
pub fn minimal_cover(table: &WeightTable, literal: &[u8]) -> Vec<Gram> {
    let mut grams = Vec::new();
    emit_cover_spans(table, literal, |span| {
        if ScanSettings::emits_len(span.len()) {
            grams.push(Gram::from(&literal[span.start..span.end]));
        }
    });
    grams
}

/// Every raw gram guaranteed to be indexed for a document containing `literal`.
pub fn guaranteed_cover(table: &WeightTable, literal: &[u8]) -> Vec<Gram> {
    let mut grams = minimal_cover(table, literal);
    engine::scan_literal(table, literal, |gram| {
        grams.push(Gram::from(&literal[gram.span.as_range()]));
    });
    grams
}

fn emit_cover_spans(table: &WeightTable, literal: &[u8], mut emit: impl FnMut(CoverSpan)) {
    let mut stack = CoverStack::new();

    for start in 0..literal.len().saturating_sub(1) {
        let weight = table.weight(literal[start], literal[start + 1]);
        stack.observe(start, weight, &mut emit);
    }
    stack.drain(&mut emit);
}

#[derive(Debug, Clone, Copy)]
struct CoverSpan {
    start: usize,
    end: usize,
}

impl CoverSpan {
    const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    const fn len(self) -> usize {
        self.end - self.start
    }
}

#[derive(Debug, Clone, Copy)]
struct CoverEntry {
    start: usize,
    weight: u32,
}

impl CoverEntry {
    const fn new(start: usize, weight: u32) -> Self {
        Self { start, weight }
    }
}

struct CoverStack {
    entries: VecDeque<CoverEntry>,
}

impl CoverStack {
    const fn new() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }

    fn observe(&mut self, start: usize, weight: u32, emit: &mut impl FnMut(CoverSpan)) {
        self.evict_front_if_too_long(start, emit);
        self.pop_lighter_back(start, weight, emit);
        self.entries.push_back(CoverEntry::new(start, weight));
    }

    fn evict_front_if_too_long(&mut self, start: usize, emit: &mut impl FnMut(CoverSpan)) {
        if self.entries.len() <= 1 {
            return;
        }
        let front = self.entries[0].start;
        if start + ScanSettings::MIN_GRAM_LEN - front < ScanSettings::MAX_GRAM_LEN {
            return;
        }
        emit(CoverSpan::new(front, self.entries[1].start + 2));
        self.entries.pop_front();
    }

    fn pop_lighter_back(&mut self, start: usize, weight: u32, emit: &mut impl FnMut(CoverSpan)) {
        while let Some(&top) = self.entries.back() {
            if weight <= top.weight {
                return;
            }
            self.glue_plateau_if_needed(top, start + 2, emit);
            self.entries.pop_back();
        }
    }

    fn glue_plateau_if_needed(
        &mut self,
        top: CoverEntry,
        end: usize,
        emit: &mut impl FnMut(CoverSpan),
    ) {
        if self.entries[0].weight != top.weight {
            return;
        }
        emit(CoverSpan::new(top.start, end));
        self.drain(emit);
    }

    fn drain(&mut self, emit: &mut impl FnMut(CoverSpan)) {
        while self.entries.len() > 1 {
            let Some(top) = self.entries.pop_back() else {
                break;
            };
            if let Some(&below) = self.entries.back() {
                emit(CoverSpan::new(below.start, top.start + 2));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn table() -> WeightTable {
        WeightTable::from_weight_fn(|c1, c2| crc32fast::hash(&[c1, c2]))
    }

    #[test]
    fn minimal_cover_produces_bounded_grams() {
        let table = table();
        for gram in minimal_cover(&table, b"MAX_FILE_SIZE") {
            assert!(ScanSettings::emits_len(gram.len()));
        }
    }

    #[test]
    fn guaranteed_cover_includes_literal_scan_grams() {
        let table = table();
        let literal = b"alpha_beta_gamma";
        let cover: HashSet<Vec<u8>> = guaranteed_cover(&table, literal)
            .into_iter()
            .map(|gram| gram.as_bytes().to_vec())
            .collect();
        engine::scan_literal(&table, literal, |gram| {
            assert!(cover.contains(&literal[gram.span.as_range()]));
        });
    }

    #[test]
    fn cover_never_emits_out_of_bounds_spans() {
        let table = table();
        let literal = b"short and longer literal";
        emit_cover_spans(&table, literal, |span| {
            assert!(span.start <= span.end);
            assert!(span.end <= literal.len());
        });
    }
}
