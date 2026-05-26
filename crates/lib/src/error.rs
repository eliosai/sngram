//! Query decomposition errors.

/// Maximum allowed regex pattern length in bytes.
pub const MAX_PATTERN_LEN: usize = 4096;

/// Errors from regex pattern decomposition.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum QueryError {
    /// No extractable literals found.
    #[error("no extractable literals in pattern")]
    NoLiterals,

    /// All literals too short for index selectivity.
    #[error("all literals shorter than minimum gram length ({min_len})")]
    LiteralsTooShort {
        /// Required minimum.
        min_len: usize,
    },

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
