//! Root scan-need derivation from regex HIR.

use crate::{ByteSet256, EdgeBytes, ScanNeed};
use regex_syntax::hir::Hir;

use super::edges::{Edge, doc_edge_literal, line_anchor_bytes};

mod classes;
mod facts;

pub struct RootNeeds {
    min_len: u64,
    single_line: bool,
    byte_counts: Option<ScanNeed>,
    any_byte_sets: Vec<ByteSet256>,
    line_start: Option<ByteSet256>,
    line_end: Option<ByteSet256>,
    starts_with: Option<EdgeBytes>,
    ends_with: Option<EdgeBytes>,
}

impl RootNeeds {
    pub fn from_hir(hir: &Hir) -> Self {
        Self {
            min_len: facts::min_match_len(hir),
            single_line: !facts::can_match_newline(hir),
            byte_counts: facts::byte_counts(hir),
            any_byte_sets: classes::required(hir),
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
        if let Some(need) = self.byte_counts {
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

#[cfg(test)]
mod tests {
    use crate::{ByteSet256, PlanExpr, ScanNeed, WeightTable};

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
