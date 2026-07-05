//! Query planning from eg patterns to Tantivy sparse-gram queries.

use anyhow::{Context, bail};
use sngram_types::{GramNeedle, PlanExpr, QueryPlan, WeightTable};
use tantivy::{
    Term,
    query::{BooleanQuery, EmptyQuery, Query, TermQuery},
    schema::{Field, IndexRecordOption},
};

use crate::flags::HiArgs;

/// A plan plus the hash key selecting the gram space its lookups use
pub(super) struct KeyedPlan {
    pub(super) plan: QueryPlan,
}

pub(super) fn query_plan(args: &HiArgs, table: &WeightTable) -> anyhow::Result<KeyedPlan> {
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
    Ok(KeyedPlan { plan })
}

pub(super) fn plan_to_query(field: Field, plan: &QueryPlan) -> anyhow::Result<Box<dyn Query>> {
    expr_to_query(field, plan.root())
}

fn expr_to_query(field: Field, expr: &PlanExpr) -> anyhow::Result<Box<dyn Query>> {
    match expr {
        PlanExpr::All => {
            bail!("indexed query has no sparse n-gram constraints; use --no-index")
        },
        PlanExpr::None => Ok(Box::new(EmptyQuery)),
        PlanExpr::AllOf {
            grams, children, ..
        } => {
            let mut clauses = Vec::with_capacity(grams.len() + children.len());
            for needle in grams {
                clauses.push(needle_query(field, needle));
            }
            for expr in children {
                clauses.push(expr_to_query(field, expr)?);
            }
            Ok(intersection_query(clauses))
        },
        PlanExpr::AnyOf {
            grams, children, ..
        } => {
            let mut clauses = Vec::with_capacity(grams.len() + children.len());
            for needle in grams {
                clauses.push(needle_query(field, needle));
            }
            for expr in children {
                clauses.push(expr_to_query(field, expr)?);
            }
            Ok(union_query(clauses))
        },
    }
}

fn needle_query(field: Field, needle: &GramNeedle) -> Box<dyn Query> {
    let clauses = needle
        .keys()
        .map(|key| term_query(field, key.value()))
        .collect();
    union_query(clauses)
}

fn term_query(field: Field, hash: u64) -> Box<dyn Query> {
    Box::new(TermQuery::new(
        Term::from_field_u64(field, hash),
        IndexRecordOption::Basic,
    ))
}

fn intersection_query(mut clauses: Vec<Box<dyn Query>>) -> Box<dyn Query> {
    match clauses.len() {
        0 => Box::new(EmptyQuery),
        1 => clauses.remove(0),
        _ => Box::new(BooleanQuery::intersection(clauses)),
    }
}

fn union_query(mut clauses: Vec<Box<dyn Query>>) -> Box<dyn Query> {
    match clauses.len() {
        0 => Box::new(EmptyQuery),
        1 => clauses.remove(0),
        _ => Box::new(BooleanQuery::union(clauses)),
    }
}
