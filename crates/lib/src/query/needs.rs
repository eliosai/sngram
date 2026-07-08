//! Root scan-need derivation from regex HIR.

use regex_syntax::hir::{Class, Hir, HirKind, Look};
use sngram_types::{ByteSet256, EdgeBytes, SaturatingByteCounts256, ScanNeed};

const MAX_ANY_BYTE_SETS: usize = 4;
const MAX_ANY_BYTE_SET_LEN: u32 = 128;

pub struct RootNeeds {
    min_len: u64,
    single_line: bool,
    byte_counts: ByteCountNeed,
    any_byte_sets: Vec<ByteSet256>,
    line_start: Option<ByteSet256>,
    line_end: Option<ByteSet256>,
    starts_with: Option<EdgeBytes>,
    ends_with: Option<EdgeBytes>,
}

impl RootNeeds {
    pub fn from_hir(hir: &Hir) -> Self {
        Self {
            min_len: min_match_len(hir),
            single_line: !can_match_newline(hir),
            byte_counts: ByteCountNeed::from_hir(hir),
            any_byte_sets: required_class_sets(hir),
            line_start: line_anchor_bytes(hir, Edge::Start),
            line_end: line_anchor_bytes(hir, Edge::End),
            starts_with: doc_edge_literal(hir, Edge::Start),
            ends_with: doc_edge_literal(hir, Edge::End),
        }
    }

    pub fn into_vec(self) -> Vec<ScanNeed> {
        let mut needs = Vec::new();
        if self.min_len > 0 {
            needs.push(ScanNeed::MinByteLen(self.min_len));
        }
        if self.single_line && self.min_len > 1 {
            let len = u32::try_from(self.min_len).unwrap_or(u32::MAX);
            needs.push(ScanNeed::MinLongestLineLen(len));
        }
        if let Some(need) = self.byte_counts.into_scan_need() {
            needs.push(need);
        }
        needs.extend(
            self.any_byte_sets
                .into_iter()
                .map(ScanNeed::ContainsAnyByte),
        );
        needs.extend(self.line_start.map(ScanNeed::LineStartsWithAnyByte));
        needs.extend(self.line_end.map(ScanNeed::LineEndsWithAnyByte));
        needs.extend(self.starts_with.map(ScanNeed::StartsWith));
        needs.extend(self.ends_with.map(ScanNeed::EndsWith));
        needs
    }
}

#[derive(Clone, Copy)]
enum Edge {
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

fn required_class_sets(hir: &Hir) -> Vec<ByteSet256> {
    let mut sets = collect_class_sets(hir);
    sets.retain(|set| {
        let len = set_len(set);
        len > 0 && len <= MAX_ANY_BYTE_SET_LEN
    });
    sets.sort_by_key(set_len);
    sets.dedup();
    sets.truncate(MAX_ANY_BYTE_SETS);
    sets
}

fn collect_class_sets(hir: &Hir) -> Vec<ByteSet256> {
    match hir.kind() {
        HirKind::Class(class) => vec![class_lead_bytes(class)],
        HirKind::Capture(capture) => collect_class_sets(&capture.sub),
        HirKind::Repetition(rep) if rep.min >= 1 => collect_class_sets(&rep.sub),
        HirKind::Empty | HirKind::Look(_) | HirKind::Literal(_) | HirKind::Repetition(_) => {
            Vec::new()
        },
        HirKind::Concat(subs) => subs.iter().flat_map(collect_class_sets).collect(),
        HirKind::Alternation(subs) => union_branch_sets(subs),
    }
}

fn union_branch_sets(subs: &[Hir]) -> Vec<ByteSet256> {
    let mut branches: Vec<Vec<ByteSet256>> = subs.iter().map(collect_class_sets).collect();
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

fn line_anchor_bytes(hir: &Hir, edge: Edge) -> Option<ByteSet256> {
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

fn doc_edge_literal(hir: &Hir, edge: Edge) -> Option<EdgeBytes> {
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

fn class_lead_bytes(class: &Class) -> ByteSet256 {
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

fn union(mut left: ByteSet256, right: ByteSet256) -> ByteSet256 {
    for (word, other) in left.words.iter_mut().zip(right.words) {
        *word |= other;
    }
    left
}

fn set_len(set: &ByteSet256) -> u32 {
    set.words.iter().map(|word| word.count_ones()).sum()
}

fn set_has(set: &ByteSet256, byte: u8) -> bool {
    set.words[usize::from(byte) / 64] >> (usize::from(byte) % 64) & 1 == 1
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

fn can_match_newline(hir: &Hir) -> bool {
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

fn min_match_len(hir: &Hir) -> u64 {
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

    fn any_byte_sets(needs: &[ScanNeed]) -> Vec<ByteSet256> {
        needs
            .iter()
            .filter_map(|need| match need {
                ScanNeed::ContainsAnyByte(set) => Some(*set),
                _ => None,
            })
            .collect()
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
    fn required_unicode_class_emits_contains_any_byte() {
        let needs = root_needs(r"read\p{Greek}lock");
        let sets = any_byte_sets(&needs);
        assert!(
            sets.iter()
                .any(|set| set_has(set, 0xCE) && !set_has(set, b'a')),
            "expected a Greek lead-byte set in {needs:?}"
        );
    }

    #[test]
    fn required_ascii_class_emits_contains_any_byte() {
        let needs = root_needs("[0-9]xyzvault");
        let sets = any_byte_sets(&needs);
        assert!(
            sets.iter()
                .any(|set| set_has(set, b'0') && set_has(set, b'9') && !set_has(set, b'x'))
        );
    }

    #[test]
    fn alternation_unions_required_class_sets() {
        let needs = root_needs("(?:[α-ω]|[0-9])suffix");
        let sets = any_byte_sets(&needs);
        assert!(
            sets.iter()
                .any(|set| set_has(set, 0xCE) && set_has(set, b'5'))
        );
    }

    #[test]
    fn optional_class_emits_no_contains_any_byte() {
        let needs = root_needs("[α-ω]*abcdef");
        assert!(any_byte_sets(&needs).is_empty());
    }

    #[test]
    fn near_full_class_sets_are_skipped() {
        let needs = root_needs("(?s:.)abcdef");
        assert!(any_byte_sets(&needs).is_empty());
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

    fn longest_line_need(needs: &[ScanNeed]) -> Option<u32> {
        needs.iter().find_map(|need| match need {
            ScanNeed::MinLongestLineLen(n) => Some(*n),
            _ => None,
        })
    }

    #[test]
    fn single_line_literal_demands_longest_line() {
        assert_eq!(longest_line_need(&root_needs("hello world")), Some(11));
    }

    #[test]
    fn single_line_gap_pattern_demands_longest_line() {
        assert_eq!(longest_line_need(&root_needs("static.*return")), Some(12));
    }

    #[test]
    fn newline_capable_dot_emits_no_longest_line() {
        assert!(longest_line_need(&root_needs("(?s)static.*return")).is_none());
    }

    #[test]
    fn newline_literal_emits_no_longest_line() {
        assert!(longest_line_need(&root_needs("foo\nbar")).is_none());
    }

    #[test]
    fn newline_class_emits_no_longest_line() {
        assert!(longest_line_need(&root_needs("foo[\n;]bar")).is_none());
    }

    #[test]
    fn unanchored_literal_emits_no_edge_needs() {
        let needs = root_needs("kfree");
        assert!(!needs.iter().any(|need| matches!(
            need,
            ScanNeed::StartsWith(_)
                | ScanNeed::EndsWith(_)
                | ScanNeed::LineStartsWithAnyByte(_)
                | ScanNeed::LineEndsWithAnyByte(_)
        )));
    }
}
