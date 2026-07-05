//! Query input validation.

use crate::types::QueryError;

use super::settings::QuerySettings;

/// Validates raw query input before planning starts.
pub struct PatternValidator;

impl PatternValidator {
    /// Validate one regex pattern.
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::PatternTooLong`] when the pattern exceeds the
    /// planner's configured byte limit.
    pub const fn validate(pattern: &str) -> Result<ValidatedPattern<'_>, QueryError> {
        if pattern.len() > QuerySettings::MAX_PATTERN_LEN {
            return Err(QueryError::PatternTooLong {
                len: pattern.len(),
                max: QuerySettings::MAX_PATTERN_LEN,
            });
        }
        Ok(ValidatedPattern { pattern })
    }
}

/// A regex pattern that passed cheap input validation.
#[derive(Debug, Clone, Copy)]
pub struct ValidatedPattern<'a> {
    pattern: &'a str,
}

impl<'a> ValidatedPattern<'a> {
    /// The validated pattern text.
    #[must_use]
    pub const fn as_str(self) -> &'a str {
        self.pattern
    }
}
