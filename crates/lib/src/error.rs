//! Query decomposition errors.

/// Maximum allowed regex pattern length in bytes.
pub const MAX_PATTERN_LEN: usize = 4096;

/// Errors from parsing a regex [`crate::Pattern`].
///
/// Analysis itself is infallible: every valid pattern yields a
/// [`crate::QueryPlan`], so these errors arise only when building the pattern.
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
