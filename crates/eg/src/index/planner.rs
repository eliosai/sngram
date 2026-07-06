//! Query planning from eg patterns to public sparse-gram plans.

use anyhow::{Context, bail};
use sngram_types::{QueryPlan, WeightTable};

use crate::flags::HiArgs;

/// A query plan plus eg-specific execution predicates.
pub(super) struct IndexPlan {
    pub(super) plan: QueryPlan,
}

impl IndexPlan {
    pub(super) fn has_gram_constraints(&self) -> bool {
        self.plan.gram_count() > 0
    }

    pub(super) fn has_root_gram_constraints(&self) -> bool {
        match self.plan.root() {
            sngram_types::PlanExpr::All | sngram_types::PlanExpr::None => false,
            sngram_types::PlanExpr::AllOf { grams, .. }
            | sngram_types::PlanExpr::AnyOf { grams, .. } => !grams.is_empty(),
        }
    }
}

pub(super) fn query_plan(args: &HiArgs, table: &WeightTable) -> anyhow::Result<IndexPlan> {
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
