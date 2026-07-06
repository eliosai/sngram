//! Query planning from eg patterns to public sparse-gram plans.

use anyhow::{Context, bail};
use sngram_types::{PlanExpr, QueryError, QueryPlan, WeightTable};

use crate::flags::HiArgs;

use super::request::Unsupported;

/// A query plan plus eg-specific execution predicates.
pub struct IndexPlan {
    pub plan: QueryPlan,
}

impl IndexPlan {
    pub fn has_gram_constraints(&self) -> bool {
        self.plan.gram_count() > 0
    }

    pub fn has_root_gram_constraints(&self) -> bool {
        match self.plan.root() {
            sngram_types::PlanExpr::All | sngram_types::PlanExpr::None => false,
            sngram_types::PlanExpr::AllOf { grams, .. }
            | sngram_types::PlanExpr::AnyOf { grams, .. } => !grams.is_empty(),
        }
    }
}

pub struct QueryPlanner<'a> {
    args: &'a HiArgs,
    table: &'a WeightTable,
}

impl<'a> QueryPlanner<'a> {
    pub const fn new(args: &'a HiArgs, table: &'a WeightTable) -> Self {
        Self { args, table }
    }

    pub fn plan(&self) -> Result<IndexPlan, PlanError> {
        let plan = query_plan(self.args, self.table).map_err(PlanError::from)?;
        match plan.plan.root() {
            PlanExpr::All => return Err(PlanError::Unsupported(Unsupported::TooBroadPattern)),
            PlanExpr::None => return Err(PlanError::Unsupported(Unsupported::ImpossiblePattern)),
            PlanExpr::AllOf { .. } | PlanExpr::AnyOf { .. } => {},
        }
        if !plan.has_gram_constraints() {
            return Err(PlanError::Unsupported(Unsupported::TooBroadPattern));
        }
        Ok(plan)
    }
}

pub enum PlanError {
    Unsupported(Unsupported),
    InvalidRegex(String),
}

impl From<anyhow::Error> for PlanError {
    fn from(err: anyhow::Error) -> Self {
        if let Some(query_err) = err.downcast_ref::<QueryError>() {
            return match query_err {
                QueryError::InvalidRegex(_) => Self::InvalidRegex(query_err.to_string()),
                QueryError::PatternTooLong { .. } => Self::Unsupported(Unsupported::PlannerError),
                _ => Self::Unsupported(Unsupported::PlannerError),
            };
        }
        log::debug!("eg index: planner rejected query: {err:#}");
        Self::Unsupported(Unsupported::PlannerError)
    }
}

pub fn query_plan(args: &HiArgs, table: &WeightTable) -> anyhow::Result<IndexPlan> {
    if args.patterns().is_empty() {
        bail!("indexed search requires at least one pattern");
    }
    let Some(pattern) = args.indexed_pattern() else {
        bail!("indexed search cannot prefilter inverted matches; use --no-index");
    };
    let plan = sngram::query(table, &pattern).with_context(|| {
        format!(
            "indexed query planner could not parse {:?}; use --no-index",
            args.patterns()
        )
    })?;
    Ok(IndexPlan { plan })
}
