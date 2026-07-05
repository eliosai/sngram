//! Public types owned by the `sngram` crate.

use core::fmt;

use sngram_types::{Gram, GramSpace, HashKey};

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

/// A conservative boolean query over sparse-gram presence.
///
/// Every document the source regex matches satisfies this plan. The plan also
/// admits non-matches, which the exact regex removes afterward.
///
/// The structure mirrors Google codesearch's `Query`: each [`QueryExpr::And`]
/// and [`QueryExpr::Or`] node carries a bag of grams alongside its sub-plans,
/// so the grams translate to a single array operation. With a postgres
/// `int8[]` column of gram hashes, an `And` bag is `grams @> ARRAY[..]` (all
/// present) and an `Or` bag is `grams && ARRAY[..]` (any present). Hash each
/// [`Gram`] with [`Self::hash_key`] so folded-space plans look up the same keys
/// [`crate::scan`] emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryPlan {
    expr: QueryExpr,
    space: GramSpace,
}

impl QueryPlan {
    /// Build a query plan from its internal expression and gram space.
    pub(crate) const fn new(expr: QueryExpr, space: GramSpace) -> Self {
        Self { expr, space }
    }

    /// The boolean expression tree for this query.
    #[must_use]
    pub const fn expr(&self) -> &QueryExpr {
        &self.expr
    }

    /// Hash space for every gram in this plan.
    #[must_use]
    pub const fn space(&self) -> GramSpace {
        self.space
    }

    /// Hash key that maps this plan's grams into the same space scan emitted.
    #[must_use]
    pub const fn hash_key(&self) -> HashKey {
        match self.space {
            GramSpace::Primary => HashKey::UNKEYED,
            GramSpace::Folded => HashKey::UNKEYED.folded(),
        }
    }

    /// True when the index cannot narrow this query.
    #[must_use]
    pub const fn is_all(&self) -> bool {
        matches!(self.expr, QueryExpr::All)
    }

    /// True when the query is provably empty.
    #[must_use]
    pub const fn is_none(&self) -> bool {
        matches!(self.expr, QueryExpr::None)
    }

    /// Total gram leaves in the plan tree.
    #[must_use]
    pub fn gram_count(&self) -> usize {
        self.expr.gram_count()
    }
}

/// Boolean sparse-gram query expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryExpr {
    /// No constraint: the index cannot narrow this query.
    All,
    /// Provably empty: no document can match.
    None,
    /// All of `grams` are present and every sub-plan holds.
    And {
        /// Grams that must all be present.
        grams: Vec<Gram>,
        /// Sub-plans that must all hold.
        sub: Vec<Self>,
    },
    /// At least one of `grams` is present, or some sub-plan holds.
    Or {
        /// Grams of which at least one must be present.
        grams: Vec<Gram>,
        /// Sub-plans of which at least one must hold.
        sub: Vec<Self>,
    },
}

impl QueryExpr {
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

    /// Total gram leaves in this expression tree.
    #[must_use]
    pub fn gram_count(&self) -> usize {
        match self {
            Self::All | Self::None => 0,
            Self::And { grams, sub } | Self::Or { grams, sub } => {
                grams.len() + sub.iter().map(Self::gram_count).sum::<usize>()
            },
        }
    }
}

/// Document-frequency statistics a deployment feeds the planner.
///
/// The provider owns unseen-gram policy: a sampled top-K stop-list provider
/// returns its best estimate (typically 0 for grams outside the sample —
/// unseen means rare). `space` tells the provider which scan hash space the
/// plan is querying.
pub trait DfStats {
    /// Estimated documents containing `gram`.
    fn doc_count(&self, space: GramSpace, gram: &Gram) -> u64;
    /// Total documents in the corpus.
    fn total_docs(&self) -> u64;
}

impl QueryPlan {
    /// Estimated candidate documents, from df priors: an `And` is bounded by
    /// its rarest member, an `Or` by the sum of its members, everything
    /// capped at the corpus size. The cost-model number that routes
    /// low-selectivity plans to a scan instead of the index.
    #[must_use]
    pub fn estimate_candidates(&self, df: &dyn DfStats) -> u64 {
        self.expr.estimate_candidates(df, self.space)
    }

    /// Reorder and thin the plan by df.
    pub fn tune(&mut self, df: &dyn DfStats, stop_df: u64) {
        self.expr.tune(df, self.space, stop_df);
    }
}

impl QueryExpr {
    fn estimate_candidates(&self, df: &dyn DfStats, space: GramSpace) -> u64 {
        let total = df.total_docs();
        match self {
            Self::All => total,
            Self::None => 0,
            Self::And { grams, sub } => {
                let g = grams.iter().map(|g| df.doc_count(space, g)).min();
                let s = sub.iter().map(|p| p.estimate_candidates(df, space)).min();
                g.into_iter().chain(s).min().unwrap_or(total)
            },
            Self::Or { grams, sub } => {
                let g: u64 = grams.iter().map(|g| df.doc_count(space, g)).sum();
                let s: u64 = sub.iter().map(|p| p.estimate_candidates(df, space)).sum();
                g.saturating_add(s).min(total)
            },
        }
    }

    fn tune(&mut self, df: &dyn DfStats, space: GramSpace, stop_df: u64) {
        match self {
            Self::All | Self::None => {},
            Self::And { grams, sub } => {
                grams.sort_by_key(|g| df.doc_count(space, g));
                let keep_first = sub.is_empty();
                let mut kept = 0usize;
                grams.retain(|g| {
                    kept += 1;
                    (keep_first && kept == 1) || df.doc_count(space, g) < stop_df
                });
                for p in sub.iter_mut() {
                    p.tune(df, space, stop_df);
                }
            },
            Self::Or { grams, sub } => {
                grams.sort_by_key(|g| df.doc_count(space, g));
                for p in sub.iter_mut() {
                    p.tune(df, space, stop_df);
                }
            },
        }
    }
}

/// Renders like codesearch's `Query.String()`.
impl fmt::Display for QueryPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.expr.fmt(f)
    }
}

/// Renders like codesearch's `Query.String()`: `+` for all, `-` for none, a
/// quoted gram for a lone leaf, space-joined for `And`, `(..)|(..)` for `Or`.
impl fmt::Display for QueryExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::All => f.write_str("+"),
            Self::None => f.write_str("-"),
            Self::And { grams, sub } => write_join(f, grams, sub, " ", false),
            Self::Or { grams, sub } => write_join(f, grams, sub, "|", true),
        }
    }
}

/// Write the grams then sub-plans of an `And`/`Or` joined by `sep`; when
/// `paren`, wrap each multi-token operand in parentheses.
fn write_join(
    f: &mut fmt::Formatter<'_>,
    grams: &[Gram],
    sub: &[QueryExpr],
    sep: &str,
    paren: bool,
) -> fmt::Result {
    let mut first = true;
    for gram in grams {
        delimit(f, &mut first, sep)?;
        write!(f, "{:?}", String::from_utf8_lossy(gram.as_bytes()))?;
    }
    for plan in sub {
        delimit(f, &mut first, sep)?;
        if paren {
            write!(f, "({plan})")?;
        } else {
            write!(f, "{plan}")?;
        }
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
