//! The plan a regex folds into

use crate::{ByteSet256, EdgeBytes, GramKey, SaturatingByteCounts256, ScanSummary};

mod display;
mod tune;

/// Why a pattern yields no plan, since the analysis of a valid pattern never fails
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum QueryError {
    /// The pattern is longer than the planner accepts
    #[error("pattern length {len} exceeds maximum {max}")]
    PatternTooLong {
        /// The pattern length
        len: usize,
        /// The longest pattern the planner accepts
        max: usize,
    },

    /// The regex parser rejected the pattern
    #[error("invalid regex: {0}")]
    InvalidRegex(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

/// A candidate plan every document the regex matches satisfies, which the verifier narrows
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryPlan {
    root: PlanExpr,
}

impl QueryPlan {
    /// A plan from its expression tree
    #[must_use]
    pub const fn new(root: PlanExpr) -> Self {
        Self { root }
    }

    /// The root expression
    #[must_use]
    pub const fn root(&self) -> &PlanExpr {
        &self.root
    }

    /// True when the index cannot narrow this query
    #[must_use]
    pub const fn is_all(&self) -> bool {
        matches!(self.root, PlanExpr::All)
    }

    /// True when no document can match
    #[must_use]
    pub const fn is_none(&self) -> bool {
        matches!(self.root, PlanExpr::None)
    }

    /// The gram needles in the whole tree
    #[must_use]
    pub fn gram_count(&self) -> usize {
        self.root.gram_count()
    }
}

/// One node of a plan, exhaustive because an executor must handle every variant
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanExpr {
    /// No constraint, the index cannot narrow this query
    All,
    /// No document can match
    None,
    /// Every gram, need and child must hold
    AllOf {
        /// The grams that must all be present
        grams: Vec<GramNeedle>,
        /// The summary conditions that must all hold
        needs: Vec<ScanNeed>,
        /// The nested expressions that must all hold
        children: Vec<Self>,
    },
    /// At least one gram, need or child must hold
    AnyOf {
        /// The grams of which one must be present
        grams: Vec<GramNeedle>,
        /// The summary conditions of which one must hold
        needs: Vec<ScanNeed>,
        /// The nested expressions of which one must hold
        children: Vec<Self>,
    },
}

impl PlanExpr {
    /// True when the index cannot narrow this expression
    #[must_use]
    pub const fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }

    /// True when no document can match this expression
    #[must_use]
    pub const fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// The gram needles in this tree
    #[must_use]
    pub fn gram_count(&self) -> usize {
        match self {
            Self::All | Self::None => 0,
            Self::AllOf {
                grams, children, ..
            }
            | Self::AnyOf {
                grams, children, ..
            } => grams.len() + children.iter().map(Self::gram_count).sum::<usize>(),
        }
    }
}

/// One gram requirement lowered to index keys, exhaustive because an executor must handle every variant
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GramNeedle {
    /// One required key
    Key(GramKey),
    /// Any one of these keys satisfies the requirement
    AnyKey(Vec<GramKey>),
    /// Any one key, with an occurrence required at a word edge
    AtWordEdge {
        /// The keys of which one must be present
        keys: Vec<GramKey>,
        /// A non-word byte or the text start must precede some occurrence
        starts: bool,
        /// A non-word byte or the text end must follow some occurrence
        ends: bool,
        /// One occurrence must carry both word edges at once
        whole: bool,
    },
    /// Any one key, with an occurrence required at a line edge
    AtLineEdge {
        /// The keys of which one must be present
        keys: Vec<GramKey>,
        /// A line break or the text start must precede some occurrence
        starts: bool,
        /// A line break or the text end must follow some occurrence
        ends: bool,
    },
}

impl GramNeedle {
    /// Every key this needle may match
    pub fn keys(&self) -> impl Iterator<Item = GramKey> + '_ {
        match self {
            Self::Key(key) => core::slice::from_ref(key).iter(),
            Self::AnyKey(keys) | Self::AtWordEdge { keys, .. } | Self::AtLineEdge { keys, .. } => {
                keys.iter()
            },
        }
        .copied()
    }
}

/// The document-frequency statistics an index feeds [`QueryPlan::tune`]
pub trait DfStats {
    /// Entries the index holds for one key
    fn entry_count(&self, key: GramKey) -> u64;
    /// Entries the index holds in all
    fn total_entries(&self) -> u64;
}

/// One condition over a [`ScanSummary`] every match needs, exhaustive because an executor must handle every variant
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanNeed {
    /// The content holds at least this many bytes
    MinByteLen(u64),
    /// The longest line holds at least this many bytes
    MinLongestLineLen(u32),
    /// At least one byte of the set occurs
    ContainsAnyByte(ByteSet256),
    /// Every byte count meets these saturating minima
    MinByteCounts(Box<SaturatingByteCounts256>),
    /// Some line starts with a byte of the set
    LineStartsWithAnyByte(ByteSet256),
    /// Some line ends with a byte of the set
    LineEndsWithAnyByte(ByteSet256),
    /// The content starts with these bytes
    StartsWith(EdgeBytes),
    /// The content ends with these bytes
    EndsWith(EdgeBytes),
}

impl ScanNeed {
    /// True when the summary satisfies this condition
    #[must_use]
    pub fn satisfied_by(&self, summary: &ScanSummary) -> bool {
        match self {
            Self::MinByteLen(n) => summary.byte_len >= *n,
            Self::MinLongestLineLen(n) => summary.longest_line_len >= *n,
            Self::ContainsAnyByte(bytes) => Self::summary_byte_set(summary).contains_any(*bytes),
            Self::MinByteCounts(counts) => summary.byte_counts.contains_at_least(counts),
            Self::LineStartsWithAnyByte(bytes) => summary.line_start_bytes.contains_any(*bytes),
            Self::LineEndsWithAnyByte(bytes) => summary.line_end_bytes.contains_any(*bytes),
            Self::StartsWith(edge) => Self::edge_prefix_matches(summary.prefix, *edge),
            Self::EndsWith(edge) => Self::edge_suffix_matches(summary.suffix, *edge),
        }
    }

    fn edge_prefix_matches(have: EdgeBytes, need: EdgeBytes) -> bool {
        have.as_slice().starts_with(need.as_slice())
    }

    fn edge_suffix_matches(have: EdgeBytes, need: EdgeBytes) -> bool {
        have.as_slice().ends_with(need.as_slice())
    }

    fn summary_byte_set(summary: &ScanSummary) -> ByteSet256 {
        let mut set = ByteSet256::default();
        for (byte, &count) in summary.byte_counts.counts().iter().enumerate() {
            if count > 0
                && let Ok(byte) = u8::try_from(byte)
            {
                set.insert(byte);
            }
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScanFlags;

    #[test]
    fn scan_need_matches_summary_bytes() {
        let mut counts = SaturatingByteCounts256::default();
        counts.observe(b'a');
        counts.observe(b'b');
        let summary = ScanSummary {
            byte_len: 2,
            line_count: 1,
            longest_line_len: 2,
            flags: ScanFlags::default().with_ascii_lower(),
            byte_counts: counts,
            line_start_bytes: one_byte(b'a'),
            line_end_bytes: one_byte(b'b'),
            prefix: edge(b"ab"),
            suffix: edge(b"ab"),
            ..Default::default()
        };

        assert!(ScanNeed::ContainsAnyByte(one_byte(b'a')).satisfied_by(&summary));
        assert!(ScanNeed::StartsWith(edge(b"a")).satisfied_by(&summary));
        assert!(ScanNeed::EndsWith(edge(b"b")).satisfied_by(&summary));
    }

    fn one_byte(byte: u8) -> ByteSet256 {
        let mut set = ByteSet256::default();
        set.insert(byte);
        set
    }

    fn edge(bytes: &[u8]) -> EdgeBytes {
        EdgeBytes::from_slice(bytes)
    }
}
