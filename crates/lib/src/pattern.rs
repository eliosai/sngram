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

pub(crate) const fn check_length(regex: &str) -> Result<(), QueryError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_source_after_successful_parse() {
        let pattern = Pattern::new(r"sched[_-]clock").expect("valid regex");
        assert_eq!(pattern.as_str(), r"sched[_-]clock");
    }

    #[test]
    fn accepts_the_documented_maximum_length() {
        let source = "a".repeat(MAX_PATTERN_LEN);
        let pattern = Pattern::new(&source).expect("pattern at limit");
        assert_eq!(pattern.as_str().len(), MAX_PATTERN_LEN);
    }

    #[test]
    fn rejects_patterns_beyond_the_documented_limit_before_parsing() {
        let source = "(".repeat(MAX_PATTERN_LEN + 1);
        let err = Pattern::new(&source).expect_err("oversized pattern");
        assert!(matches!(
            err,
            QueryError::PatternTooLong {
                len,
                max: MAX_PATTERN_LEN
            } if len == MAX_PATTERN_LEN + 1
        ));
    }

    #[test]
    fn invalid_regex_reports_parse_error() {
        let err = Pattern::new("[").expect_err("invalid regex");
        assert!(matches!(err, QueryError::InvalidRegex(_)));
    }
}
