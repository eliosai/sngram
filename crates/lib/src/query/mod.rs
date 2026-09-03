//! Regex analysis into a sparse-gram plan

mod algebra;
mod analyze;
mod combine;
mod covers;
mod edges;
mod flush;
mod gram;
mod info;
mod needs;
mod order;
mod parser;
mod pattern;
pub mod plan;
mod planner;
mod settings;
mod strings;
mod validate;

use crate::{QueryError, QueryPlan, WeightTable};

/// Fold one regex into the plan every document it matches satisfies
pub fn query(table: &WeightTable, pattern: &str) -> Result<QueryPlan, QueryError> {
    let pattern = validate::validate(pattern)?;
    planner::QueryPlanner::new(table).plan(pattern)
}
