use crate::{SaturatingByteCounts256, ScanNeed};
use regex_syntax::hir::{Class, Hir, HirKind};

pub fn byte_counts(hir: &Hir) -> Option<ScanNeed> {
    ByteCountNeed::from_hir(hir).into_scan_need()
}

#[derive(Clone, Copy, Default)]
struct ByteCountNeed {
    counts: SaturatingByteCounts256,
}

impl ByteCountNeed {
    fn from_hir(hir: &Hir) -> Self {
        match hir.kind() {
            HirKind::Empty | HirKind::Look(_) | HirKind::Class(_) => Self::default(),
            HirKind::Literal(lit) => Self::from_literal(&lit.0),
            HirKind::Repetition(rep) => Self::from_hir(&rep.sub).repeated(rep.min),
            HirKind::Capture(capture) => Self::from_hir(&capture.sub),
            HirKind::Concat(subs) => Self::from_concat(subs),
            HirKind::Alternation(subs) => Self::from_alternation(subs),
        }
    }

    fn from_literal(bytes: &[u8]) -> Self {
        let mut need = Self::default();
        for &byte in bytes {
            need.counts.observe(byte);
        }
        need
    }

    fn from_concat(subs: &[Hir]) -> Self {
        subs.iter()
            .map(Self::from_hir)
            .fold(Self::default(), |mut acc, need| {
                acc.add(need);
                acc
            })
    }

    fn from_alternation(subs: &[Hir]) -> Self {
        let Some((first, rest)) = subs.split_first() else {
            return Self::default();
        };
        let mut acc = Self::from_hir(first);
        for sub in rest {
            acc.keep_branch_min(Self::from_hir(sub));
        }
        acc
    }

    fn repeated(mut self, min: u32) -> Self {
        for count in &mut self.counts.counts {
            *count = repeat_count(*count, min);
        }
        self
    }

    fn add(&mut self, other: Self) {
        for (left, right) in self.counts.counts.iter_mut().zip(other.counts.counts) {
            *left = left.saturating_add(right);
        }
    }

    fn keep_branch_min(&mut self, other: Self) {
        for (left, right) in self.counts.counts.iter_mut().zip(other.counts.counts) {
            *left = (*left).min(right);
        }
    }

    fn into_scan_need(self) -> Option<ScanNeed> {
        (!self.counts.is_empty()).then_some(ScanNeed::MinByteCounts(Box::new(self.counts)))
    }
}

fn repeat_count(count: u8, times: u32) -> u8 {
    let product = u32::from(count).saturating_mul(times);
    u8::try_from(product).unwrap_or(u8::MAX)
}

pub fn can_match_newline(hir: &Hir) -> bool {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => false,
        HirKind::Literal(lit) => lit.0.contains(&b'\n'),
        HirKind::Class(class) => class_has_newline(class),
        HirKind::Repetition(rep) => rep.max != Some(0) && can_match_newline(&rep.sub),
        HirKind::Capture(capture) => can_match_newline(&capture.sub),
        HirKind::Concat(subs) | HirKind::Alternation(subs) => subs.iter().any(can_match_newline),
    }
}

fn class_has_newline(class: &Class) -> bool {
    match class {
        Class::Bytes(bytes) => bytes
            .ranges()
            .iter()
            .any(|r| r.start() <= b'\n' && b'\n' <= r.end()),
        Class::Unicode(chars) => chars
            .ranges()
            .iter()
            .any(|r| r.start() <= '\n' && '\n' <= r.end()),
    }
}

pub fn min_match_len(hir: &Hir) -> u64 {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => 0,
        HirKind::Literal(lit) => u64::try_from(lit.0.len()).unwrap_or(u64::MAX),
        HirKind::Class(_) => 1,
        HirKind::Repetition(rep) => u64::from(rep.min).saturating_mul(min_match_len(&rep.sub)),
        HirKind::Capture(capture) => min_match_len(&capture.sub),
        HirKind::Concat(subs) => subs
            .iter()
            .map(min_match_len)
            .fold(0u64, u64::saturating_add),
        HirKind::Alternation(subs) => subs.iter().map(min_match_len).min().unwrap_or(0),
    }
}
