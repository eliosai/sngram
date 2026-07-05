//! Regex parsing for query planning.

use regex_syntax::hir::Hir;

use crate::types::QueryError;

use super::{pattern::PatternFacts, settings::QuerySettings, validate::ValidatedPattern};

/// Parsed query input plus lightweight facts that are not preserved in HIR.
pub struct ParsedPattern {
    hir: Hir,
    facts: PatternFacts,
}

impl ParsedPattern {
    /// Regex HIR for planning.
    #[must_use]
    pub const fn hir(&self) -> &Hir {
        &self.hir
    }

    /// Whether planning should use the folded gram space.
    #[must_use]
    pub const fn uses_folded_space(&self) -> bool {
        self.facts.uses_folded_space()
    }
}

/// Parses a validated pattern into regex HIR.
pub struct QueryParser;

impl QueryParser {
    /// Parse one regex pattern with the planner's internal defaults.
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::InvalidRegex`] when regex-syntax rejects the
    /// pattern.
    pub fn parse(pattern: ValidatedPattern<'_>) -> Result<ParsedPattern, QueryError> {
        let pattern = pattern.as_str();
        let facts = PatternFacts::analyze(pattern);
        let hir = regex_syntax::ParserBuilder::new()
            .nest_limit(QuerySettings::VERIFIER_NEST_LIMIT)
            .octal(QuerySettings::OCTAL)
            .utf8(QuerySettings::UTF8)
            .multi_line(QuerySettings::MULTI_LINE)
            .case_insensitive(QuerySettings::CASE_INSENSITIVE)
            .dot_matches_new_line(QuerySettings::DOT_MATCHES_NEW_LINE)
            .crlf(QuerySettings::CRLF)
            .unicode(QuerySettings::UNICODE)
            .build()
            .parse(pattern)
            .map_err(|err| QueryError::InvalidRegex(Box::new(err)))?;
        Ok(ParsedPattern { hir, facts })
    }
}
