//! Query planning types.

use core::fmt;

use crate::{ByteSet256, EdgeBytes, GramKey, SaturatingByteCounts256, ScanFlags, ScanSummary};

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
        self.root.tune(df, stop_df);
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
}

impl GramNeedle {
    /// Iterate all concrete keys this logical needle may match.
    pub fn keys(&self) -> impl Iterator<Item = GramKey> + '_ {
        match self {
            Self::Key(key) => core::slice::from_ref(key).iter(),
            Self::AnyKey(keys) => keys.iter(),
        }
        .copied()
    }

    fn estimate_candidates(&self, df: &dyn DfStats) -> u64 {
        let total = df.total_entries();
        match self {
            Self::Key(key) => df.entry_count(*key).min(total),
            Self::AnyKey(keys) => keys
                .iter()
                .map(|&key| df.entry_count(key))
                .sum::<u64>()
                .min(total),
        }
    }

    fn sort_keys_by_df(&mut self, df: &dyn DfStats) {
        if let Self::AnyKey(keys) = self {
            keys.sort_by_cached_key(|&key| df.entry_count(key));
            keys.dedup();
        }
    }
}

/// A necessary condition over [`ScanSummary`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanNeed {
    /// Content length must be at least this many bytes.
    MinByteLen(u64),
    /// Line count must be at least this value.
    MinLineCount(u32),
    /// Empty-line count must be at least this value.
    MinEmptyLineCount(u32),
    /// Longest line must be at least this many bytes.
    MinLongestLineLen(u32),
    /// All flags must be present.
    HasFlags(ScanFlags),
    /// All bytes in the set must occur somewhere.
    ContainsAllBytes(ByteSet256),
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
            Self::MinLineCount(n) => summary.line_count >= *n,
            Self::MinEmptyLineCount(n) => summary.empty_line_count >= *n,
            Self::MinLongestLineLen(n) => summary.longest_line_len >= *n,
            Self::HasFlags(flags) => summary.flags.contains(*flags),
            Self::ContainsAllBytes(bytes) => Self::summary_byte_set(summary).contains_all(*bytes),
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
    const MAX_ALL_OF_GRAMS: usize = 32;

    fn tune(&mut self, df: &dyn DfStats, stop_df: u64) {
        match self {
            Self::All | Self::None => {},
            Self::AllOf {
                grams, children, ..
            } => {
                let keep_first = children.is_empty();
                Self::sort_grams_by_df(grams, df);
                Self::retain_selective_grams(grams, keep_first, df, stop_df);
                Self::tune_children(children, df, stop_df);
            },
            Self::AnyOf {
                grams, children, ..
            } => {
                Self::sort_grams_by_df(grams, df);
                Self::tune_children(children, df, stop_df);
            },
        }
    }

    fn retain_selective_grams(
        grams: &mut Vec<GramNeedle>,
        keep_first: bool,
        df: &dyn DfStats,
        stop_df: u64,
    ) {
        let mut kept = 0usize;
        grams.retain(|g| {
            kept += 1;
            ((keep_first && kept == 1) || g.estimate_candidates(df) < stop_df)
                && kept <= Self::MAX_ALL_OF_GRAMS
        });
    }

    fn sort_grams_by_df(grams: &mut [GramNeedle], df: &dyn DfStats) {
        for needle in grams.iter_mut() {
            needle.sort_keys_by_df(df);
        }
        grams.sort_by_cached_key(|g| g.estimate_candidates(df));
    }

    fn tune_children(children: &mut [Self], df: &dyn DfStats, stop_df: u64) {
        for child in children {
            child.tune(df, stop_df);
        }
    }

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
            Self::MinLineCount(n) => write!(f, "MinLineCount({n})"),
            Self::MinEmptyLineCount(n) => write!(f, "MinEmptyLineCount({n})"),
            Self::MinLongestLineLen(n) => write!(f, "MinLongestLineLen({n})"),
            Self::HasFlags(flags) => write!(f, "HasFlags({flags:?})"),
            Self::ContainsAllBytes(bytes) => write!(f, "ContainsAllBytes({bytes:?})"),
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
    use std::collections::HashMap;

    use super::*;

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

        assert!(ScanNeed::ContainsAllBytes(one_byte(b'a')).satisfied_by(&summary));
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

    struct MapDf {
        counts: HashMap<GramKey, u64>,
        total: u64,
    }

    impl DfStats for MapDf {
        fn entry_count(&self, key: GramKey) -> u64 {
            self.counts.get(&key).copied().unwrap_or(0)
        }

        fn total_entries(&self) -> u64 {
            self.total
        }
    }

    fn df_of(pairs: &[(GramKey, u64)], total: u64) -> MapDf {
        MapDf {
            counts: pairs.iter().copied().collect(),
            total,
        }
    }

    fn key(value: u64) -> GramKey {
        GramKey(value)
    }

    #[test]
    fn gram_estimates_bound_and_by_rarest_or_by_sum() {
        let df = df_of(&[(key(1), 900), (key(2), 3)], 1000);
        let and = PlanExpr::AllOf {
            grams: vec![GramNeedle::Key(key(1)), GramNeedle::Key(key(2))],
            needs: vec![],
            children: vec![],
        };
        let or = PlanExpr::AnyOf {
            grams: vec![GramNeedle::Key(key(1)), GramNeedle::Key(key(2))],
            needs: vec![],
            children: vec![],
        };

        assert_eq!(estimate_candidates(&and, &df), 3);
        assert_eq!(estimate_candidates(&or, &df), 903);
        assert_eq!(estimate_candidates(&PlanExpr::All, &df), 1000);
        assert_eq!(estimate_candidates(&PlanExpr::None, &df), 0);
    }

    #[test]
    fn tuning_caps_all_of_grams_to_rarest_few() {
        let df = df_of(
            &[
                (key(1), 10),
                (key(2), 20),
                (key(3), 30),
                (key(4), 40),
                (key(5), 50),
            ],
            1000,
        );
        let mut plan = QueryPlan::new(PlanExpr::AllOf {
            grams: vec![
                GramNeedle::Key(key(5)),
                GramNeedle::Key(key(4)),
                GramNeedle::Key(key(3)),
                GramNeedle::Key(key(2)),
                GramNeedle::Key(key(1)),
            ],
            needs: vec![],
            children: vec![],
        });

        plan.tune(&df, 45);

        let PlanExpr::AllOf { grams, .. } = plan.root() else {
            panic!("tuned plan must stay AllOf");
        };
        assert_eq!(
            grams,
            &[
                GramNeedle::Key(key(1)),
                GramNeedle::Key(key(2)),
                GramNeedle::Key(key(3)),
                GramNeedle::Key(key(4)),
            ]
        );
    }

    fn estimate_candidates(expr: &PlanExpr, df: &dyn DfStats) -> u64 {
        let total = df.total_entries();
        match expr {
            PlanExpr::All => total,
            PlanExpr::None => 0,
            PlanExpr::AllOf {
                grams, children, ..
            } => estimate_all_candidates(grams, children, df),
            PlanExpr::AnyOf {
                grams, children, ..
            } => estimate_any_candidates(grams, children, df),
        }
    }

    fn estimate_all_candidates(
        grams: &[GramNeedle],
        children: &[PlanExpr],
        df: &dyn DfStats,
    ) -> u64 {
        let grams = grams.iter().map(|gram| gram.estimate_candidates(df)).min();
        let children = children
            .iter()
            .map(|child| estimate_candidates(child, df))
            .min();
        grams
            .into_iter()
            .chain(children)
            .min()
            .unwrap_or_else(|| df.total_entries())
    }

    fn estimate_any_candidates(
        grams: &[GramNeedle],
        children: &[PlanExpr],
        df: &dyn DfStats,
    ) -> u64 {
        let grams = grams
            .iter()
            .map(|gram| gram.estimate_candidates(df))
            .fold(0u64, u64::saturating_add);
        let children = children
            .iter()
            .map(|child| estimate_candidates(child, df))
            .fold(0u64, u64::saturating_add);
        grams.saturating_add(children).min(df.total_entries())
    }
}
