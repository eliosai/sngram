//! Query planning from eg patterns to public sparse-gram plans.

use anyhow::{Context, bail};
use sngram::{ByteSet256, PlanExpr, QueryError, QueryPlan, ScanNeed, WeightTable};

use crate::flags::HiArgs;

use super::request::Unsupported;

/// A query plan plus eg-specific execution predicates.
pub struct IndexPlan {
    pub plan: QueryPlan,
    pub precision: super::executor::Precision,
}

impl IndexPlan {
    pub fn has_gram_constraints(&self) -> bool {
        self.plan.gram_count() > 0
    }

    pub fn has_root_gram_constraints(&self) -> bool {
        match self.plan.root() {
            sngram::PlanExpr::All | sngram::PlanExpr::None => false,
            sngram::PlanExpr::AllOf { grams, .. } | sngram::PlanExpr::AnyOf { grams, .. } => {
                !grams.is_empty()
            },
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
    if args.multiline() {
        return Ok(IndexPlan {
            plan,
            precision: super::executor::Precision::Doc,
        });
    }
    Ok(IndexPlan {
        plan: line_scoped(plan),
        precision: super::executor::Precision::Block,
    })
}

/// Weaken content-edge needs into line-edge needs.
///
/// Line-by-line search hands the matcher one line at a time, so `\A` and `\z`
/// bind to a line, not to the file. A content need would drop files the
/// verifier still reports a match in.
fn line_scoped(plan: QueryPlan) -> QueryPlan {
    if has_content_edge_need(plan.root()) {
        return QueryPlan::new(line_scoped_expr(plan.root()));
    }
    plan
}

/// True when any need in the tree binds to the content edges
fn has_content_edge_need(expr: &PlanExpr) -> bool {
    let (PlanExpr::AllOf {
        needs, children, ..
    }
    | PlanExpr::AnyOf {
        needs, children, ..
    }) = expr
    else {
        return false;
    };
    needs
        .iter()
        .any(|need| matches!(need, ScanNeed::StartsWith(_) | ScanNeed::EndsWith(_)))
        || children.iter().any(has_content_edge_need)
}

fn line_scoped_expr(expr: &PlanExpr) -> PlanExpr {
    match expr {
        PlanExpr::All | PlanExpr::None => expr.clone(),
        PlanExpr::AllOf {
            grams,
            needs,
            children,
        } => PlanExpr::AllOf {
            grams: grams.clone(),
            needs: needs.iter().map(line_scoped_need).collect(),
            children: children.iter().map(line_scoped_expr).collect(),
        },
        PlanExpr::AnyOf {
            grams,
            needs,
            children,
        } => PlanExpr::AnyOf {
            grams: grams.clone(),
            needs: needs.iter().map(line_scoped_need).collect(),
            children: children.iter().map(line_scoped_expr).collect(),
        },
    }
}

/// A line-scoped need implied by a content-edge need
fn line_scoped_need(need: &ScanNeed) -> ScanNeed {
    match need {
        ScanNeed::StartsWith(edge) => edge_byte_need(edge.as_slice().first(), true),
        ScanNeed::EndsWith(edge) => edge_byte_need(edge.as_slice().last(), false),
        other => other.clone(),
    }
}

/// The line-edge need one required edge byte implies, or a vacuous need
fn edge_byte_need(byte: Option<&u8>, starts: bool) -> ScanNeed {
    let Some(&byte) = byte else {
        return ScanNeed::MinByteLen(0);
    };
    let mut set = ByteSet256::default();
    set.insert(byte);
    if starts {
        ScanNeed::LineStartsWithAnyByte(set)
    } else {
        ScanNeed::LineEndsWithAnyByte(set)
    }
}

#[cfg(test)]
mod tests {
    use sngram::{ByteSet256, PlanExpr, QueryPlan, ScanNeed};

    use super::line_scoped;

    fn plan(pattern: &str) -> QueryPlan {
        sngram::query(sngram::weights(), pattern).expect("pattern plans")
    }

    fn all_needs(expr: &PlanExpr) -> Vec<ScanNeed> {
        let (PlanExpr::AllOf {
            needs, children, ..
        }
        | PlanExpr::AnyOf {
            needs, children, ..
        }) = expr
        else {
            return Vec::new();
        };
        let mut found = needs.clone();
        found.extend(children.iter().flat_map(all_needs));
        found
    }

    fn byte_set(byte: u8) -> ByteSet256 {
        let mut set = ByteSet256::default();
        set.insert(byte);
        set
    }

    #[test]
    fn content_start_need_weakens_to_a_line_start_need() {
        let scoped = line_scoped(plan(r"\A#include"));
        let found = all_needs(scoped.root());

        assert!(
            !found
                .iter()
                .any(|need| matches!(need, ScanNeed::StartsWith(_)))
        );
        assert!(found.contains(&ScanNeed::LineStartsWithAnyByte(byte_set(b'#'))));
    }

    #[test]
    fn content_end_need_weakens_to_a_line_end_need() {
        let scoped = line_scoped(plan(r"return 0;\z"));
        let found = all_needs(scoped.root());

        assert!(
            !found
                .iter()
                .any(|need| matches!(need, ScanNeed::EndsWith(_)))
        );
        assert!(found.contains(&ScanNeed::LineEndsWithAnyByte(byte_set(b';'))));
    }

    #[test]
    fn plans_without_content_edges_are_left_alone() {
        let original = plan("^kfree_skb");

        assert_eq!(original, line_scoped(original.clone()));
    }
}
