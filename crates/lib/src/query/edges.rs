//! Anchored edge-byte and edge-literal derivation from regex HIR.

use regex_syntax::hir::{Class, Hir, HirKind, Look};
use sngram_types::{ByteSet256, EdgeBytes};

/// Which content edge an anchor binds.
#[derive(Clone, Copy)]
pub enum Edge {
    Start,
    End,
}

impl Edge {
    const fn line_look(self, look: Look) -> bool {
        match self {
            Self::Start => matches!(look, Look::StartLF | Look::Start),
            Self::End => matches!(look, Look::EndLF | Look::End),
        }
    }

    const fn doc_look(self) -> Look {
        match self {
            Self::Start => Look::Start,
            Self::End => Look::End,
        }
    }

    fn ordered<'a>(self, subs: &'a [Hir]) -> Box<dyn Iterator<Item = &'a Hir> + 'a> {
        match self {
            Self::Start => Box::new(subs.iter()),
            Self::End => Box::new(subs.iter().rev()),
        }
    }
}

pub fn line_anchor_bytes(hir: &Hir, edge: Edge) -> Option<ByteSet256> {
    let set = anchored_edge_bytes(hir, edge)?;
    (!set.is_empty() && !set_has(&set, b'\n')).then_some(set)
}

fn anchored_edge_bytes(hir: &Hir, edge: Edge) -> Option<ByteSet256> {
    match hir.kind() {
        HirKind::Capture(capture) => anchored_edge_bytes(&capture.sub, edge),
        HirKind::Repetition(rep) if rep.min >= 1 => anchored_edge_bytes(&rep.sub, edge),
        HirKind::Alternation(subs) => subs
            .iter()
            .map(|sub| anchored_edge_bytes(sub, edge))
            .try_fold(ByteSet256::default(), |acc, set| Some(union(acc, set?))),
        HirKind::Concat(subs) => anchored_concat_bytes(subs, edge),
        _ => None,
    }
}

fn anchored_concat_bytes(subs: &[Hir], edge: Edge) -> Option<ByteSet256> {
    let mut elems = edge.ordered(subs);
    match elems.next()?.kind() {
        HirKind::Look(look) if edge.line_look(*look) => {},
        _ => return None,
    }
    let mut set = ByteSet256::default();
    for elem in elems {
        let bytes = edge_bytes(elem, edge);
        set = union(set, bytes.set);
        if !bytes.can_be_empty {
            return Some(set);
        }
    }
    None
}

struct EdgeByteSet {
    set: ByteSet256,
    can_be_empty: bool,
}

fn edge_bytes(hir: &Hir, edge: Edge) -> EdgeByteSet {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => empty_edge(),
        HirKind::Literal(lit) => literal_edge(&lit.0, edge),
        HirKind::Class(class) => EdgeByteSet {
            set: class_edge_bytes(class, edge),
            can_be_empty: false,
        },
        HirKind::Capture(capture) => edge_bytes(&capture.sub, edge),
        HirKind::Repetition(rep) => repetition_edge(rep, edge),
        HirKind::Concat(subs) => concat_edge(subs, edge),
        HirKind::Alternation(subs) => alternation_edge(subs, edge),
    }
}

fn empty_edge() -> EdgeByteSet {
    EdgeByteSet {
        set: ByteSet256::default(),
        can_be_empty: true,
    }
}

fn literal_edge(bytes: &[u8], edge: Edge) -> EdgeByteSet {
    let byte = match edge {
        Edge::Start => bytes.first(),
        Edge::End => bytes.last(),
    };
    let mut set = ByteSet256::default();
    if let Some(&byte) = byte {
        set.insert(byte);
    }
    EdgeByteSet {
        set,
        can_be_empty: bytes.is_empty(),
    }
}

fn repetition_edge(rep: &regex_syntax::hir::Repetition, edge: Edge) -> EdgeByteSet {
    let mut bytes = edge_bytes(&rep.sub, edge);
    if rep.min == 0 {
        bytes.can_be_empty = true;
    }
    bytes
}

fn concat_edge(subs: &[Hir], edge: Edge) -> EdgeByteSet {
    let mut set = ByteSet256::default();
    for elem in edge.ordered(subs) {
        let bytes = edge_bytes(elem, edge);
        set = union(set, bytes.set);
        if !bytes.can_be_empty {
            return EdgeByteSet {
                set,
                can_be_empty: false,
            };
        }
    }
    EdgeByteSet {
        set,
        can_be_empty: true,
    }
}

fn alternation_edge(subs: &[Hir], edge: Edge) -> EdgeByteSet {
    let mut acc = EdgeByteSet {
        set: ByteSet256::default(),
        can_be_empty: false,
    };
    for bytes in subs.iter().map(|sub| edge_bytes(sub, edge)) {
        acc.set = union(acc.set, bytes.set);
        acc.can_be_empty = acc.can_be_empty || bytes.can_be_empty;
    }
    acc
}

pub fn doc_edge_literal(hir: &Hir, edge: Edge) -> Option<EdgeBytes> {
    let HirKind::Concat(subs) = hir.kind() else {
        return None;
    };
    let mut elems = edge.ordered(subs);
    match elems.next()?.kind() {
        HirKind::Look(look) if *look == edge.doc_look() => {},
        _ => return None,
    }
    let HirKind::Literal(lit) = elems.next()?.kind() else {
        return None;
    };
    let bytes = &lit.0;
    let taken = match edge {
        Edge::Start => &bytes[..bytes.len().min(EdgeBytes::CAPACITY)],
        Edge::End => &bytes[bytes.len().saturating_sub(EdgeBytes::CAPACITY)..],
    };
    (!taken.is_empty()).then(|| EdgeBytes::from_slice(taken))
}

pub fn class_lead_bytes(class: &Class) -> ByteSet256 {
    class_edge_bytes(class, Edge::Start)
}

fn class_edge_bytes(class: &Class, edge: Edge) -> ByteSet256 {
    let mut set = ByteSet256::default();
    match class {
        Class::Bytes(bytes) => {
            for range in bytes.ranges() {
                insert_range(&mut set, range.start(), range.end());
            }
        },
        Class::Unicode(chars) => {
            for range in chars.ranges() {
                insert_scalar_range(&mut set, range.start(), range.end(), edge);
            }
        },
    }
    set
}

fn insert_scalar_range(set: &mut ByteSet256, start: char, end: char, edge: Edge) {
    match edge {
        Edge::Start => insert_range(set, utf8_lead(start), utf8_lead(end)),
        Edge::End => {
            if start.is_ascii() {
                insert_range(
                    set,
                    start as u8,
                    if end.is_ascii() { end as u8 } else { 0x7F },
                );
            }
            if !end.is_ascii() {
                insert_range(set, 0x80, 0xBF);
            }
        },
    }
}

fn utf8_lead(c: char) -> u8 {
    let mut buf = [0u8; 4];
    c.encode_utf8(&mut buf).as_bytes()[0]
}

fn insert_range(set: &mut ByteSet256, start: u8, end: u8) {
    for byte in start..=end {
        set.insert(byte);
    }
}

pub fn union(mut left: ByteSet256, right: ByteSet256) -> ByteSet256 {
    for (word, other) in left.words.iter_mut().zip(right.words) {
        *word |= other;
    }
    left
}

pub fn set_len(set: &ByteSet256) -> u32 {
    set.words.iter().map(|word| word.count_ones()).sum()
}

fn set_has(set: &ByteSet256, byte: u8) -> bool {
    set.words[usize::from(byte) / 64] >> (usize::from(byte) % 64) & 1 == 1
}

#[cfg(test)]
mod tests {
    use sngram_types::{ByteSet256, PlanExpr, ScanNeed, WeightTable};

    use crate::query::query;

    fn table() -> WeightTable {
        WeightTable::from_weight_fn(|c1, c2| crc32fast::hash(&[c1, c2]))
    }

    fn root_needs(re: &str) -> Vec<ScanNeed> {
        let plan = query(&table(), re).expect("pattern plans");
        match plan.root() {
            PlanExpr::AllOf { needs, .. } => needs.clone(),
            _ => Vec::new(),
        }
    }

    fn set_has(set: &ByteSet256, byte: u8) -> bool {
        set.words[usize::from(byte) / 64] >> (usize::from(byte) % 64) & 1 == 1
    }

    fn line_start_set(needs: &[ScanNeed]) -> Option<ByteSet256> {
        needs.iter().find_map(|need| match need {
            ScanNeed::LineStartsWithAnyByte(set) => Some(*set),
            _ => None,
        })
    }

    fn line_end_set(needs: &[ScanNeed]) -> Option<ByteSet256> {
        needs.iter().find_map(|need| match need {
            ScanNeed::LineEndsWithAnyByte(set) => Some(*set),
            _ => None,
        })
    }

    #[test]
    fn line_start_anchor_emits_first_byte_set() {
        let set = line_start_set(&root_needs("^kfree")).expect("line-start need");
        assert!(set_has(&set, b'k'));
        assert!(!set_has(&set, b'f'));
    }

    #[test]
    fn anchored_alternation_unions_first_bytes() {
        let set = line_start_set(&root_needs("^int|^long")).expect("line-start need");
        assert!(set_has(&set, b'i') && set_has(&set, b'l'));
    }

    #[test]
    fn partially_anchored_alternation_emits_nothing() {
        assert!(line_start_set(&root_needs("kfree|^int")).is_none());
    }

    #[test]
    fn anchored_leading_class_contributes_all_first_bytes() {
        let set = line_start_set(&root_needs("^[ \t]+return")).expect("line-start need");
        assert!(set_has(&set, b' ') && set_has(&set, b'\t'));
    }

    #[test]
    fn newline_capable_first_byte_emits_nothing() {
        assert!(line_start_set(&root_needs("^\nfoo")).is_none());
    }

    #[test]
    fn line_end_anchor_emits_last_byte_set() {
        let set = line_end_set(&root_needs("foo_bar$")).expect("line-end need");
        assert!(set_has(&set, b'r'));
        assert!(!set_has(&set, b'o'));
    }

    #[test]
    fn optional_tail_widens_line_end_set() {
        let set = line_end_set(&root_needs("foo;?$")).expect("line-end need");
        assert!(set_has(&set, b';') && set_has(&set, b'o'));
    }

    #[test]
    fn doc_start_anchor_emits_starts_with() {
        let needs = root_needs(r"\A#include");
        assert!(needs.iter().any(|need| matches!(
            need,
            ScanNeed::StartsWith(edge) if edge.as_slice() == b"#include"
        )));
    }

    #[test]
    fn doc_end_anchor_emits_ends_with() {
        let needs = root_needs(r"return 0;\z");
        assert!(needs.iter().any(|need| matches!(
            need,
            ScanNeed::EndsWith(edge) if edge.as_slice() == b"return 0;"
        )));
    }
}
