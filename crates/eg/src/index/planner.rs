//! Query planning from eg patterns to Tantivy sparse-gram queries.

use anyhow::{Context, bail};
use sngram::{Pattern, QueryPlan};
use sngram_types::WeightTable;
use tantivy::{
    Term,
    query::{BooleanQuery, EmptyQuery, Query, TermQuery},
    schema::{Field, IndexRecordOption},
};

use crate::flags::HiArgs;

pub(super) fn query_plan(args: &HiArgs, table: &WeightTable) -> anyhow::Result<QueryPlan> {
    let source = planning_regex(args)?;
    let pattern = Pattern::new(&source).with_context(|| {
        format!("indexed query planner could not parse {source:?}; use --no-index")
    })?;
    let plan = sngram::query(table, &pattern);
    if matches!(plan, QueryPlan::All) {
        bail!("indexed query has no sparse n-gram constraints; use --no-index");
    }
    Ok(plan)
}

pub(super) fn plan_to_query(field: Field, plan: &QueryPlan) -> anyhow::Result<Box<dyn Query>> {
    match plan {
        QueryPlan::All => bail!("indexed query has no sparse n-gram constraints; use --no-index"),
        QueryPlan::None => Ok(Box::new(EmptyQuery)),
        QueryPlan::And { grams, sub } => {
            let mut clauses = Vec::with_capacity(grams.len() + sub.len());
            for gram in grams {
                clauses.push(term_query(field, gram.hash()));
            }
            for plan in sub {
                clauses.push(plan_to_query(field, plan)?);
            }
            Ok(intersection_query(clauses))
        },
        QueryPlan::Or { grams, sub } => {
            let mut clauses = Vec::with_capacity(grams.len() + sub.len());
            for gram in grams {
                clauses.push(term_query(field, gram.hash()));
            }
            for plan in sub {
                clauses.push(plan_to_query(field, plan)?);
            }
            Ok(union_query(clauses))
        },
    }
}

fn planning_regex(args: &HiArgs) -> anyhow::Result<String> {
    let patterns = args.patterns();
    if patterns.is_empty() {
        bail!("indexed search requires at least one pattern");
    }
    let case_insensitive = args.indexed_case_insensitive();
    let mut parts = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        let source = if args.fixed_strings() {
            regex_syntax::escape(pattern)
        } else {
            pattern.clone()
        };
        if case_insensitive {
            parts.push(format!("(?i:{source})"));
        } else {
            parts.push(format!("(?:{source})"));
        }
    }
    Ok(if parts.len() == 1 {
        parts.remove(0)
    } else {
        parts.join("|")
    })
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
