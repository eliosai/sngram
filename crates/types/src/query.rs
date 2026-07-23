//! Query planning types.

use core::fmt;

use crate::{ByteSet256, EdgeBytes, GramKey, SaturatingByteCounts256, ScanSummary, tuning};

/// Errors from parsing query patterns.
///
/// Analysis itself is infallible: every valid pattern yields a
/// [`QueryPlan`], so these errors arise only when building the pattern.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum QueryError {
    /// Input pattern exceeds size limit.
    #[error("pattern length {len} exceeds maximum {max}")]
    PatternTooLong {
        /// Actual length.
        len: usize,
        /// Limit.
        max: usize,
    },

    /// Invalid regex syntax.
    #[error("invalid regex: {0}")]
    InvalidRegex(#[from] Box<regex_syntax::Error>),
}

/// A conservative candidate plan over gram keys and scan-derived metadata.
///
/// Every indexed text entry matched by the source regex satisfies this plan.
/// The plan admits non-matches; the exact regex verifier removes those later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryPlan {
    root: PlanExpr,
}

impl QueryPlan {
    /// Build a query plan from its expression tree.
    #[must_use]
    pub const fn new(root: PlanExpr) -> Self {
        Self { root }
    }

    /// The root expression for this plan.
    #[must_use]
    pub const fn root(&self) -> &PlanExpr {
        &self.root
    }

    /// True when the index cannot narrow this query.
    #[must_use]
    pub const fn is_all(&self) -> bool {
        matches!(self.root, PlanExpr::All)
    }

    /// True when the query is provably empty.
    #[must_use]
    pub const fn is_none(&self) -> bool {
        matches!(self.root, PlanExpr::None)
    }

    /// Total gram needles in the plan tree.
    #[must_use]
    pub fn gram_count(&self) -> usize {
        self.root.gram_count()
    }

    /// Reorder and thin the plan by df.
    pub fn tune(&mut self, df: &dyn DfStats, stop_df: u64) {
        tuning::tune(&mut self.root, df, stop_df);
    }
}

/// Boolean candidate expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanExpr {
    /// No constraint: the index cannot narrow this query.
    All,
    /// Provably empty: no indexed text entry can match.
    None,
    /// Every gram, scan need, and child must hold.
    AllOf {
        /// Gram-key requirements.
        grams: Vec<GramNeedle>,
        /// Scan-summary requirements.
        needs: Vec<ScanNeed>,
        /// Nested expressions that must also hold.
        children: Vec<Self>,
    },
    /// At least one gram, scan need, or child must hold.
    AnyOf {
        /// Gram-key alternatives.
        grams: Vec<GramNeedle>,
        /// Scan-summary alternatives.
        needs: Vec<ScanNeed>,
        /// Nested alternatives.
        children: Vec<Self>,
    },
}

impl PlanExpr {
    /// True when the index cannot narrow this expression.
    #[must_use]
    pub const fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }

    /// True when this expression is provably empty.
    #[must_use]
    pub const fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// Total gram needles in this expression tree.
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

/// One logical gram requirement, already lowered to final index keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GramNeedle {
    /// One required key.
    Key(GramKey),
    /// Any one of these keys satisfies the gram requirement.
    AnyKey(Vec<GramKey>),
    /// Any one key, with occurrences required at word edges.
    AtWordEdge {
        /// Alternative keys for the gram.
        keys: Vec<GramKey>,
        /// A non-word byte or text start must precede some occurrence.
        starts: bool,
        /// A non-word byte or text end must follow some occurrence.
        ends: bool,
        /// One single occurrence must carry both word edges at once.
        whole: bool,
    },
}

impl GramNeedle {
    /// Iterate all concrete keys this logical needle may match.
    pub fn keys(&self) -> impl Iterator<Item = GramKey> + '_ {
        match self {
            Self::Key(key) => core::slice::from_ref(key).iter(),
            Self::AnyKey(keys) | Self::AtWordEdge { keys, .. } => keys.iter(),
        }
        .copied()
    }
}

/// A necessary condition over [`ScanSummary`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanNeed {
    /// Content length must be at least this many bytes.
    MinByteLen(u64),
    /// Longest line must be at least this many bytes.
    MinLongestLineLen(u32),
    /// At least one byte in the set must occur somewhere.
    ContainsAnyByte(ByteSet256),
    /// Byte counts must meet these saturating minima.
    MinByteCounts(Box<SaturatingByteCounts256>),
    /// At least one line must start with a byte in the set.
    LineStartsWithAnyByte(ByteSet256),
    /// At least one line must end with a byte in the set.
    LineEndsWithAnyByte(ByteSet256),
    /// Content must start with these bytes.
    StartsWith(EdgeBytes),
    /// Content must end with these bytes.
    EndsWith(EdgeBytes),
}

impl ScanNeed {
    /// True when a scan summary satisfies this necessary condition.
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
        for (byte, &count) in summary.byte_counts.counts.iter().enumerate() {
            if count > 0
                && let Ok(byte) = u8::try_from(byte)
            {
                set.insert(byte);
            }
        }
        set
    }
}

/// Document-frequency statistics a deployment feeds the planner.
pub trait DfStats {
    /// Estimated entries containing this concrete gram key.
    fn entry_count(&self, key: GramKey) -> u64;
    /// Total indexed text entries.
    fn total_entries(&self) -> u64;
}

impl PlanExpr {
    fn write_all_of(
        f: &mut fmt::Formatter<'_>,
        grams: &[GramNeedle],
        needs: &[ScanNeed],
        children: &[Self],
    ) -> fmt::Result {
        let mut first = true;
        for gram in grams {
            Self::delimit(f, &mut first, " ")?;
            write!(f, "{gram:?}")?;
        }
        for need in needs {
            Self::delimit(f, &mut first, " ")?;
            write!(f, "{need}")?;
        }
        for child in children {
            Self::delimit(f, &mut first, " ")?;
            write!(f, "{child}")?;
        }
        Ok(())
    }

    fn write_any_of(
        f: &mut fmt::Formatter<'_>,
        grams: &[GramNeedle],
        needs: &[ScanNeed],
        children: &[Self],
    ) -> fmt::Result {
        let mut first = true;
        for gram in grams {
            Self::delimit(f, &mut first, "|")?;
            write!(f, "{gram:?}")?;
        }
        for need in needs {
            Self::delimit(f, &mut first, "|")?;
            write!(f, "{need}")?;
        }
        for child in children {
            Self::delimit(f, &mut first, "|")?;
            write!(f, "({child})")?;
        }
        Ok(())
    }

    fn delimit(f: &mut fmt::Formatter<'_>, first: &mut bool, sep: &str) -> fmt::Result {
        if *first {
            *first = false;
            Ok(())
        } else {
            f.write_str(sep)
        }
    }
}

impl fmt::Display for QueryPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.root.fmt(f)
    }
}

impl fmt::Display for ScanNeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MinByteLen(n) => write!(f, "MinByteLen({n})"),
            Self::MinLongestLineLen(n) => write!(f, "MinLongestLineLen({n})"),
            Self::ContainsAnyByte(bytes) => write!(f, "ContainsAnyByte({bytes:?})"),
            Self::MinByteCounts(counts) => write_byte_counts(f, counts),
            Self::LineStartsWithAnyByte(bytes) => write!(f, "LineStartsWithAnyByte({bytes:?})"),
            Self::LineEndsWithAnyByte(bytes) => write!(f, "LineEndsWithAnyByte({bytes:?})"),
            Self::StartsWith(edge) => write!(f, "StartsWith({edge:?})"),
            Self::EndsWith(edge) => write!(f, "EndsWith({edge:?})"),
        }
    }
}

fn write_byte_counts(f: &mut fmt::Formatter<'_>, counts: &SaturatingByteCounts256) -> fmt::Result {
    f.write_str("MinByteCounts(")?;
    let mut first = true;
    for (byte, count) in counts
        .counts
        .iter()
        .enumerate()
        .filter(|&(_, &count)| count > 0)
    {
        PlanExpr::delimit(f, &mut first, ",")?;
        write!(f, "{byte:#04x}:{count}")?;
    }
    f.write_str(")")
}

impl fmt::Display for PlanExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::All => f.write_str("+"),
            Self::None => f.write_str("-"),
            Self::AllOf {
                grams,
                needs,
                children,
            } => Self::write_all_of(f, grams, needs, children),
            Self::AnyOf {
                grams,
                needs,
                children,
            } => Self::write_any_of(f, grams, needs, children),
        }
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
            empty_line_count: 0,
            longest_line_len: 2,
            gram_count: 0,
            flags: ScanFlags::default().with_ascii_lower(),
            byte_counts: counts,
            line_start_bytes: one_byte(b'a'),
            line_end_bytes: one_byte(b'b'),
            prefix: edge(b"ab"),
            suffix: edge(b"ab"),
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
