//! Regex pattern parsing into a reusable HIR.

use regex_syntax::hir::Hir;

use crate::error::{MAX_PATTERN_LEN, QueryError};

const NEST_LIMIT: u32 = 100;

/// Validated regex pattern with pre-parsed HIR.
#[derive(Debug, Clone)]
pub struct Pattern {
    source: String,
    hir: Hir,
}

impl Pattern {
    /// # Errors
    ///
    /// Returns `QueryError` on invalid syntax or oversized input.
    pub fn new(regex: &str) -> Result<Self, QueryError> {
        check_length(regex)?;
        let hir = parse(regex)?;
        Ok(Self {
            source: regex.to_owned(),
            hir,
        })
    }

    /// Original regex string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// The parsed HIR, the input to query analysis.
    pub(crate) const fn hir(&self) -> &Hir {
        &self.hir
    }
}

const fn check_length(regex: &str) -> Result<(), QueryError> {
    if regex.len() > MAX_PATTERN_LEN {
        return Err(QueryError::PatternTooLong {
            len: regex.len(),
            max: MAX_PATTERN_LEN,
        });
    }
    Ok(())
}

fn parse(regex: &str) -> Result<Hir, QueryError> {
    regex_syntax::ParserBuilder::new()
        .nest_limit(NEST_LIMIT)
        .build()
        .parse(regex)
        .map_err(|e| QueryError::InvalidRegex(Box::new(e)))
}
