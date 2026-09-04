//! Literal covering grams for query planning.

use std::collections::VecDeque;
use std::ops::Range;

use crate::WeightTable;

use super::engine;
use super::settings::ScanSettings;
use super::space::SpaceScanner;

/// A covering pass over short literals, reused across a whole flush.
///
/// The monotonic stack and the literal scanner are the pass's carried state;
/// both are rewound per literal, so a pass over thousands of branches builds
/// each of them once. The scanner waits for the first guaranteed cover, since
/// a pass that only chains minimal covers never runs it.
pub struct Cover<'t> {
    table: &'t WeightTable,
    stack: CoverStack,
    scanner: Option<SpaceScanner<'t>>,
}

impl<'t> Cover<'t> {
    /// A pass bound to `table`.
    pub const fn new(table: &'t WeightTable) -> Self {
        Self {
            table,
            stack: CoverStack::new(),
            scanner: None,
        }
    }

    /// Visit the span of each minimal covering gram, the chain that covers
    /// `literal` end to end.
    pub fn each_minimal_span(&mut self, literal: &[u8], mut visit: impl FnMut(Range<usize>)) {
        self.stack.reset();
        for start in 0..literal.len().saturating_sub(1) {
            let weight = self.table.weight(literal[start], literal[start + 1]);
            self.stack.observe(start, weight, &mut emitter(&mut visit));
        }
        self.stack.drain(&mut emitter(&mut visit));
    }

    /// Visit the span of every raw gram guaranteed to be indexed for a
    /// document containing `literal`.
    pub fn each_guaranteed_span(&mut self, literal: &[u8], mut visit: impl FnMut(Range<usize>)) {
        self.each_minimal_span(literal, &mut visit);
        let table = self.table;
        let scanner = self
            .scanner
            .get_or_insert_with(|| engine::literal_scanner(table));
        scanner.restart();
        scanner.push_bytes(literal, literal.len(), &mut |gram| {
            visit(gram.span.as_range());
        });
    }
}

/// Adapt a span visitor to the emitted-length filter the index applies.
fn emitter(visit: &mut impl FnMut(Range<usize>)) -> impl FnMut(CoverSpan) + '_ {
    move |span| {
        if ScanSettings::emits_len(span.len()) {
            visit(span.start..span.end);
        }
    }
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

    fn reset(&mut self) {
        self.entries.clear();
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

    fn spans(literal: &[u8], guaranteed: bool) -> Vec<Range<usize>> {
        let table = table();
        let mut cover = Cover::new(&table);
        let mut out = Vec::new();
        if guaranteed {
            cover.each_guaranteed_span(literal, |at| out.push(at));
        } else {
            cover.each_minimal_span(literal, |at| out.push(at));
        }
        out
    }

    #[test]
    fn minimal_cover_produces_bounded_grams() {
        for at in spans(b"MAX_FILE_SIZE", false) {
            assert!(ScanSettings::emits_len(at.len()));
        }
    }

    #[test]
    fn guaranteed_cover_includes_literal_scan_grams() {
        let table = table();
        let literal = b"alpha_beta_gamma";
        let cover: HashSet<Vec<u8>> = spans(literal, true)
            .into_iter()
            .map(|at| literal[at].to_vec())
            .collect();
        let mut scanner = engine::literal_scanner(&table);
        scanner.push_bytes(literal, literal.len(), &mut |gram| {
            assert!(cover.contains(&literal[gram.span.as_range()]));
        });
    }

    #[test]
    fn cover_never_emits_out_of_bounds_spans() {
        let literal = b"short and longer literal";
        for at in spans(literal, false) {
            assert!(at.start <= at.end);
            assert!(at.end <= literal.len());
        }
    }

    /// A literal that wraps the scanner's prefix ring before the given length
    fn wrapping_literal(len: usize) -> Vec<u8> {
        (0..len)
            .map(|at| b"abcdefghijklmnopqrstuvwxyz_0123"[at % 31])
            .collect()
    }

    #[test]
    fn a_reused_pass_matches_a_fresh_one() {
        let table = table();
        let mut shared = Cover::new(&table);
        for len in [300usize, 16, 127, 128, 129, 3, 400, 5] {
            let literal = wrapping_literal(len);
            let mut reused = Vec::new();
            shared.each_guaranteed_span(&literal, |at| reused.push(at));
            assert_eq!(reused, spans(&literal, true), "length {len}");
        }
        let literals: [&[u8]; 2] = [b"alpha_beta_gamma", b"MAX_FILE_SIZE"];
        for literal in literals {
            let mut reused = Vec::new();
            shared.each_guaranteed_span(literal, |at| reused.push(at));
            assert_eq!(reused, spans(literal, true));
        }
    }
}
