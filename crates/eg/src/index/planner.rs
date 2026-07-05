//! Query planning from eg patterns to Tantivy sparse-gram queries.

use anyhow::{Context, bail};
use sngram::types::{QueryExpr, QueryPlan};
use sngram_types::HashKey;
use sngram_types::WeightTable;
use tantivy::{
    Term,
    query::{BooleanQuery, EmptyQuery, Query, TermQuery},
    schema::{Field, IndexRecordOption},
};

use crate::flags::HiArgs;

/// A plan plus the hash key selecting the gram space its lookups use
pub(super) struct KeyedPlan {
    pub(super) plan: QueryPlan,
    pub(super) key: HashKey,
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
    let key = plan.hash_key();
    Ok(KeyedPlan { plan, key })
}

pub(super) fn plan_to_query(
    field: Field,
    plan: &QueryPlan,
    key: HashKey,
) -> anyhow::Result<Box<dyn Query>> {
    expr_to_query(field, plan.expr(), key)
}

fn expr_to_query(field: Field, expr: &QueryExpr, key: HashKey) -> anyhow::Result<Box<dyn Query>> {
    match expr {
        QueryExpr::All => {
            bail!("indexed query has no sparse n-gram constraints; use --no-index")
        },
        QueryExpr::None => Ok(Box::new(EmptyQuery)),
        QueryExpr::And { grams, sub } => {
            let mut clauses = Vec::with_capacity(grams.len() + sub.len());
            for gram in grams {
                clauses.push(term_query(field, gram.hash_keyed(key)));
            }
            for expr in sub {
                clauses.push(expr_to_query(field, expr, key)?);
            }
            Ok(intersection_query(clauses))
        },
        QueryExpr::Or { grams, sub } => {
            let mut clauses = Vec::with_capacity(grams.len() + sub.len());
            for gram in grams {
                clauses.push(term_query(field, gram.hash_keyed(key)));
            }
            for expr in sub {
                clauses.push(expr_to_query(field, expr, key)?);
            }
            Ok(union_query(clauses))
        },
    }
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
