//! Regex pattern parsing with prefix+suffix literal extraction.

use regex_syntax::hir::literal::{ExtractKind, Extractor};
use regex_syntax::hir::Hir;

use crate::error::{QueryError, MAX_PATTERN_LEN};

const MIN_LITERAL_LEN: usize = 3;
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
        Ok(Self { source: regex.to_owned(), hir })
    }

    /// Original regex string.
    #[must_use]
    pub fn as_str(&self) -> &str { &self.source }

    /// # Errors
    ///
    /// Returns `QueryError` if no usable literals can be extracted.
    pub(crate) fn extract_literals(&self) -> Result<Vec<Vec<u8>>, QueryError> {
        let literals = extract_both(&self.hir)?;
        validate_lengths(&literals)?;
        Ok(literals)
    }
}

const fn check_length(regex: &str) -> Result<(), QueryError> {
    if regex.len() > MAX_PATTERN_LEN {
        return Err(QueryError::PatternTooLong { len: regex.len(), max: MAX_PATTERN_LEN });
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

fn extract_both(hir: &Hir) -> Result<Vec<Vec<u8>>, QueryError> {
    let prefixes = Extractor::new().kind(ExtractKind::Prefix).extract(hir);
    let suffixes = Extractor::new().kind(ExtractKind::Suffix).extract(hir);

    let mut all = Vec::new();
    collect_literals(&prefixes, &mut all);
    collect_literals(&suffixes, &mut all);
    all.sort();
    all.dedup();

    if all.is_empty() { return Err(QueryError::NoLiterals); }
    Ok(all)
}

fn collect_literals(seq: &regex_syntax::hir::literal::Seq, out: &mut Vec<Vec<u8>>) {
    if let Some(lits) = seq.literals() {
        for lit in lits {
            let bytes = lit.as_bytes();
            if !bytes.is_empty() {
                out.push(bytes.to_vec());
            }
        }
    }
}

fn validate_lengths(literals: &[Vec<u8>]) -> Result<(), QueryError> {
    if literals.iter().any(|l| l.len() >= MIN_LITERAL_LEN) {
        return Ok(());
    }
    Err(QueryError::LiteralsTooShort { min_len: MIN_LITERAL_LEN })
}
