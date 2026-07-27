use regex_syntax::hir::{Hir, HirKind};
use sngram_types::ByteSet256;

use crate::query::edges::{class_lead_bytes, set_len, union};

const MAX_ANY_BYTE_SETS: usize = 4;
const MAX_ANY_BYTE_SET_LEN: u32 = 128;

pub fn required(hir: &Hir) -> Vec<ByteSet256> {
    let mut sets = collect(hir);
    sets.retain(|set| {
        let len = set_len(set);
        len > 0 && len <= MAX_ANY_BYTE_SET_LEN
    });
    sets.sort_by_key(set_len);
    sets.dedup();
    sets.truncate(MAX_ANY_BYTE_SETS);
    sets
}

fn collect(hir: &Hir) -> Vec<ByteSet256> {
    match hir.kind() {
        HirKind::Class(class) => vec![class_lead_bytes(class)],
        HirKind::Capture(capture) => collect(&capture.sub),
        HirKind::Repetition(rep) if rep.min >= 1 => collect(&rep.sub),
        HirKind::Empty | HirKind::Look(_) | HirKind::Literal(_) | HirKind::Repetition(_) => {
            Vec::new()
        },
        HirKind::Concat(subs) => subs.iter().flat_map(collect).collect(),
        HirKind::Alternation(subs) => union_branches(subs),
    }
}

fn union_branches(subs: &[Hir]) -> Vec<ByteSet256> {
    let mut branches: Vec<Vec<ByteSet256>> = subs.iter().map(collect).collect();
    for branch in &mut branches {
        branch.sort_by_key(set_len);
    }
    let shortest = branches.iter().map(Vec::len).min().unwrap_or(0);
    (0..shortest)
        .map(|i| {
            branches
                .iter()
                .fold(ByteSet256::default(), |acc, branch| union(acc, branch[i]))
        })
        .collect()
}
