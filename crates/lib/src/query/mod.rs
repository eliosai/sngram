//! Regex to sparse-gram query analysis.
//!
//! A regex is folded bottom-up into a [`QueryPlan`]: a conservative boolean
//! query over gram presence that every matching document must satisfy. The
//! plan over-approximates (false positives are fine; a real match is never
//! dropped), so it prefilters candidates before the exact regex runs.

mod algebra;
mod analyze;
mod combine;
mod covers;
mod edges;
mod flush;
mod info;
mod needs;
mod parser;
mod pattern;
mod planner;
mod settings;
mod strings;
mod validate;

use sngram_types::{QueryError, QueryPlan, WeightTable};

/// Decompose one regex pattern into a sparse-gram query plan.
///
/// CLI concerns such as fixed-string escaping, multi-pattern OR joining,
/// smart case, inversion, CRLF, and byte-regex mode should be encoded by the
/// caller into this single pattern before calling the library.
///
/// # Errors
///
/// Returns [`QueryError`] when `pattern` exceeds the length limit or fails to
/// parse.
pub fn query(table: &WeightTable, pattern: &str) -> Result<QueryPlan, QueryError> {
    let pattern = validate::PatternValidator::validate(pattern)?;
    planner::QueryPlanner::new(table).plan(pattern)
}
