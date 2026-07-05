//! Query planning from eg patterns to Tantivy sparse-gram queries.

use anyhow::{Context, bail};
use sngram::{GramSpace, HashKey, QueryPlan};
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
    let patterns = args.patterns();
    if patterns.is_empty() {
        bail!("indexed search requires at least one pattern");
    }
    let opts = sngram::QueryOptions {
        folded_space: true,
        line_sentinels: true,
        ..args.query_options()
    };
    let planned = sngram::query(table, patterns, opts).with_context(|| {
        format!("indexed query planner could not parse {patterns:?}; use --no-index")
    })?;
    let key = match planned.space {
        GramSpace::Primary => HashKey::UNKEYED,
        GramSpace::Folded => HashKey::UNKEYED.folded(),
    };
    Ok(KeyedPlan {
        plan: planned.plan,
        key,
    })
}

pub(super) fn plan_to_query(
    field: Field,
    plan: &QueryPlan,
    key: HashKey,
) -> anyhow::Result<Box<dyn Query>> {
    match plan {
        QueryPlan::All => bail!("indexed query has no sparse n-gram constraints; use --no-index"),
        QueryPlan::None => Ok(Box::new(EmptyQuery)),
        QueryPlan::And { grams, sub } => {
            let mut clauses = Vec::with_capacity(grams.len() + sub.len());
            for gram in grams {
                clauses.push(term_query(field, gram.hash_keyed(key)));
            }
            for plan in sub {
                clauses.push(plan_to_query(field, plan, key)?);
            }
            Ok(intersection_query(clauses))
        },
        QueryPlan::Or { grams, sub } => {
            let mut clauses = Vec::with_capacity(grams.len() + sub.len());
            for gram in grams {
                clauses.push(term_query(field, gram.hash_keyed(key)));
            }
            for plan in sub {
                clauses.push(plan_to_query(field, plan, key)?);
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
