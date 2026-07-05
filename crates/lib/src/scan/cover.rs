//! Literal covering grams for query planning.

use std::collections::VecDeque;

use sngram_types::{Gram, WeightTable};

use crate::scan::{engine, settings::ScanSettings};

/// Minimal covering grams of a single literal.
pub fn minimal_cover(table: &WeightTable, literal: &[u8]) -> Vec<Gram> {
    let mut grams = Vec::new();
    emit_cover_spans(table, literal, |start, end| {
        if (ScanSettings::MIN_LEN..=ScanSettings::MAX_LEN).contains(&(end - start)) {
            grams.push(Gram::from(&literal[start..end]));
        }
    });
    grams
}

/// Every raw gram guaranteed to be indexed for a document containing `literal`.
pub fn guaranteed_cover(table: &WeightTable, literal: &[u8]) -> Vec<Gram> {
    let mut grams = minimal_cover(table, literal);
    engine::scan_literal(table, literal, |gram| {
        grams.push(Gram::from(gram.bytes));
    });
    grams
}

fn emit_cover_spans(table: &WeightTable, literal: &[u8], mut emit: impl FnMut(usize, usize)) {
    let mut stack: VecDeque<(u32, usize)> = VecDeque::new();

    for start in 0..literal.len().saturating_sub(1) {
        let weight = table.weight(literal[start], literal[start + 1]);
        if stack.len() > 1 && start + 3 - stack[0].1 >= ScanSettings::MAX_LEN {
            emit(stack[0].1, stack[1].1 + 2);
            stack.pop_front();
        }
        while let Some(&(top_weight, top_start)) = stack.back() {
            if weight <= top_weight {
                break;
            }
            if stack[0].0 == top_weight {
                glue_plateau(&mut stack, top_start, start + 2, &mut emit);
            }
            stack.pop_back();
        }
        stack.push_back((weight, start));
    }
    drain(&mut stack, &mut emit);
}

fn glue_plateau(
    stack: &mut VecDeque<(u32, usize)>,
    back_pos: usize,
    end: usize,
    emit: &mut impl FnMut(usize, usize),
) {
    emit(back_pos, end);
    drain(stack, emit);
}

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
            assert!((ScanSettings::MIN_LEN..=ScanSettings::MAX_LEN).contains(&gram.len()));
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
            assert!(cover.contains(gram.bytes));
        });
    }

    #[test]
    fn cover_never_emits_out_of_bounds_spans() {
        let table = table();
        let literal = b"short and longer literal";
        emit_cover_spans(&table, literal, |start, end| {
            assert!(start <= end);
            assert!(end <= literal.len());
        });
    }
}
