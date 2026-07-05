//! Public types owned by the `sngram` crate.

use core::fmt;

use sngram_types::{Gram, HashKey};

pub const STACK_CAP: usize = 128;
pub const RING: usize = 128;
pub const WINDOW_CAP: usize = 1024;

/// Maximum allowed regex pattern length in bytes.
pub const MAX_PATTERN_LEN: usize = 4096;

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

/// Errors from [`crate::scan`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ScanError {}

/// Scan-time format options; index build and query plan must agree on them.
///
/// `key` selects the hash space (see [`HashKey`]); `fold` scans the
/// ASCII-case-folded stream into the folded twin space (the emitted hashes are
/// automatically tagged with [`HashKey::folded`]); `line_sentinels` brackets
/// every document with a virtual `\n` so anchored line-boundary grams exist at
/// the document's first and last line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScanOptions {
    /// Deployment hash key for the emitted gram hashes.
    pub key: HashKey,
    /// Bracket the document with virtual line terminators.
    pub line_sentinels: bool,
    /// Scan the ASCII-folded stream, emitting into the folded twin space.
    pub fold: bool,
}

/// One sparse n-gram emitted by [`crate::scan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScannedGram<'a> {
    /// Gram bytes in the scanned space.
    ///
    /// When [`ScanOptions::fold`] is enabled these bytes are ASCII-folded. When
    /// [`ScanOptions::line_sentinels`] is enabled they may include the virtual
    /// `\n` boundary bytes.
    pub bytes: &'a [u8],
    /// Start offset in the scanned stream.
    pub start: usize,
    /// End offset in the scanned stream.
    pub end: usize,
    /// Hash key for this gram in the selected scan space.
    pub hash: u64,
}

/// Streaming sparse n-gram extraction that holds a bounded window, never the whole document.
pub struct StreamScanner<'t> {
    pub(crate) matrix: &'t [u32; 65536],
    pub(crate) window: [u8; WINDOW_CAP],
    pub(crate) wlen: usize,
    pub(crate) base: usize,
    /// monotonic stack of (absolute position, weight); positions are unbounded
    /// in a stream, so entries stay unpacked
    pub(crate) stack: [(usize, u32); STACK_CAP],
    pub(crate) slen: usize,
    /// rolling prefix hash of everything pushed since the last `finish`
    pub(crate) hash: u64,
    /// recent prefix-hash values `H[p]`, indexed by absolute position masked
    pub(crate) ring: [u64; RING],
    /// effective hash space: the deployment key, fold-tagged when folding
    pub(crate) ekey: HashKey,
    pub(crate) opts: ScanOptions,
    /// whether the current document received its leading sentinel
    pub(crate) started: bool,
}

/// How the verifying engine interprets query pattern text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum QuerySyntax {
    /// Rust `regex` crate syntax, the engine default.
    #[default]
    Regex,
    /// Patterns are literal strings (`grep -F`); metacharacters are escaped
    /// before parsing, exactly as `grep-regex` does.
    FixedStrings,
}

/// The verifying engine's query case mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum QueryCase {
    /// Match case sensitively.
    #[default]
    Sensitive,
    /// Match case insensitively.
    Insensitive,
    /// Insensitive when no pattern contains an uppercase literal
    /// (`ripgrep -S`); resolved here with the same rule `grep-regex` applies,
    /// so the plan and the verifier can never disagree.
    Smart,
}

/// Options for [`crate::query`].
///
/// Matcher fields must mirror the engine that verifies candidates, or the plan
/// may drop real matches. The index-format fields must mirror the way documents
/// were scanned, or the returned gram hashes may target a space the index does
/// not contain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool mirrors an independent matcher flag; a state machine would misrepresent them"
)]
pub struct QueryOptions {
    /// Pattern text interpretation.
    pub syntax: QuerySyntax,
    /// Case mode, including engine-rule smart case.
    pub case: QueryCase,
    /// Unicode mode (`--no-unicode` disables).
    pub unicode: bool,
    /// Whether `.` matches line terminators (`-U --multiline-dotall`).
    /// Currently plan-inert — `.` over-approximates to any character either
    /// way — but kept so the planner's HIR always equals the verifier's.
    pub dotall: bool,
    /// CRLF-aware anchors (`--crlf`).
    pub crlf: bool,
    /// Match-sense inversion (`-v`). The only sound prefilter for "lines
    /// that do NOT match" is every document, so this forces
    /// [`QueryPlan::All`].
    pub invert: bool,
    /// The index carries a folded twin space for every gram.
    pub folded_space: bool,
    /// Documents were scanned with virtual line sentinels.
    pub line_sentinels: bool,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            syntax: QuerySyntax::default(),
            case: QueryCase::default(),
            unicode: true,
            dotall: false,
            crlf: false,
            invert: false,
            folded_space: false,
            line_sentinels: false,
        }
    }
}

/// A conservative boolean query over sparse-gram presence.
///
/// Every document the source regex matches satisfies this plan. The plan also
/// admits non-matches, which the exact regex removes afterward.
///
/// The structure mirrors Google codesearch's `Query`: each [`Self::And`] and
/// [`Self::Or`] node carries a bag of grams alongside its sub-plans, so the
/// grams translate to a single array operation. With a postgres `int4[]`
/// column of gram hashes, an `And` bag is `grams @> ARRAY[..]` (all present)
/// and an `Or` bag is `grams && ARRAY[..]` (any present). Each [`Gram`]'s
/// 64-bit key comes from [`Gram::hash`], matching the keys [`crate::scan`]
/// emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryPlan {
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

/// Which hash space a plan's grams key into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GramSpace {
    /// The primary space: raw document bytes.
    Primary,
    /// The folded twin space: ASCII-case-folded bytes, hashes tagged with
    /// [`HashKey::folded`].
    Folded,
}

/// A plan plus the space its grams must be hashed into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedQuery {
    /// The boolean gram query.
    pub plan: QueryPlan,
    /// Hash space for every gram in the plan.
    pub space: GramSpace,
}

/// Document-frequency statistics a deployment feeds the planner.
///
/// The provider owns key hashing and unseen-gram policy: a sampled top-K
/// stop-list provider returns its best estimate (typically 0 for grams
/// outside the sample — unseen means rare).
pub trait DfStats {
    /// Estimated documents containing `gram`.
    fn doc_count(&self, gram: &Gram) -> u64;
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
        let total = df.total_docs();
        match self {
            Self::All => total,
            Self::None => 0,
            Self::And { grams, sub } => {
                let g = grams.iter().map(|g| df.doc_count(g)).min();
                let s = sub.iter().map(|p| p.estimate_candidates(df)).min();
                g.into_iter().chain(s).min().unwrap_or(total)
            },
            Self::Or { grams, sub } => {
                let g: u64 = grams.iter().map(|g| df.doc_count(g)).sum();
                let s: u64 = sub.iter().map(|p| p.estimate_candidates(df)).sum();
                g.saturating_add(s).min(total)
            },
        }
    }

    /// Reorder and thin the plan by df: `And` bags sort rarest-first and drop
    /// stop grams (df at or above `stop_df`) while at least one discriminator
    /// remains. Dropping an `And` member only widens the plan, so tuning is
    /// always sound; `Or` bags are never thinned (every branch must stay
    /// covered) but sort for stable output.
    pub fn tune(&mut self, df: &dyn DfStats, stop_df: u64) {
        match self {
            Self::All | Self::None => {},
            Self::And { grams, sub } => {
                grams.sort_by_key(|g| df.doc_count(g));
                let keep_first = sub.is_empty();
                let mut kept = 0usize;
                grams.retain(|g| {
                    kept += 1;
                    (keep_first && kept == 1) || df.doc_count(g) < stop_df
                });
                for p in sub.iter_mut() {
                    p.tune(df, stop_df);
                }
            },
            Self::Or { grams, sub } => {
                grams.sort_by_key(|g| df.doc_count(g));
                for p in sub.iter_mut() {
                    p.tune(df, stop_df);
                }
            },
        }
    }
}

/// Renders like codesearch's `Query.String()`: `+` for all, `-` for none, a
/// quoted gram for a lone leaf, space-joined for `And`, `(..)|(..)` for `Or`.
impl fmt::Display for QueryPlan {
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
    sub: &[QueryPlan],
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
