//! Query planning from regex HIR to public plans.

use regex_syntax::hir::{Hir, HirKind, Look};
use sngram_types::{
    Gram, GramKey, GramNeedle, HashKey, PlanExpr, QueryError, QueryPlan, ScanNeed, WeightTable,
};

use super::{
    algebra::{Op, Query},
    analyze::{Analyzer, PlanContext, is_word_byte},
    needs::RootNeeds,
    parser::QueryParser,
    settings::QuerySettings,
    strings::StringSet,
    validate::ValidatedPattern,
};

/// Builds sparse-gram query plans against one weight table.
pub struct QueryPlanner<'a> {
    table: &'a WeightTable,
}

impl<'a> QueryPlanner<'a> {
    /// Bind the planner to a weight table.
    #[must_use]
    pub const fn new(table: &'a WeightTable) -> Self {
        Self { table }
    }

    /// Plan one validated regex pattern.
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::InvalidRegex`] when regex parsing fails.
    pub fn plan(&self, pattern: ValidatedPattern<'_>) -> Result<QueryPlan, QueryError> {
        let parsed = QueryParser::parse(pattern)?;
        let fold = QuerySettings::CASE_FOLDED_SUPPLEMENTS && parsed.uses_folded_space();
        let ctx = PlanContext {
            fold,
            line_sentinels: QuerySettings::LINE_SENTINELS,
        };
        Ok(QueryPlan::new(self.plan_hir(parsed.hir(), ctx)))
    }

    fn plan_hir(&self, hir: &Hir, ctx: PlanContext) -> PlanExpr {
        let analyzer = Analyzer::with_context(self.table, ctx);
        let edges = if ctx.fold {
            None
        } else {
            word_edged_literal(hir)
        };
        with_root_needs(
            into_public_expr(analyzer.plan(hir), ctx.fold, edges),
            RootNeeds::from_hir(hir),
        )
    }
}

fn with_root_needs(expr: PlanExpr, needs: RootNeeds) -> PlanExpr {
    let needs = needs.into_vec();
    if needs.is_empty() || expr.is_none() {
        return expr;
    }
    append_root_needs(expr, needs)
}

fn append_root_needs(expr: PlanExpr, new_needs: Vec<ScanNeed>) -> PlanExpr {
    let (grams, needs, children) = match expr {
        PlanExpr::All => (vec![], new_needs, vec![]),
        PlanExpr::AllOf {
            grams,
            mut needs,
            children,
        } => {
            needs.extend(new_needs);
            (grams, needs, children)
        },
        other => (vec![], new_needs, vec![other]),
    };
    PlanExpr::AllOf {
        grams,
        needs,
        children,
    }
}

fn into_public_expr(query: Query, fold: bool, edges: Option<&[u8]>) -> PlanExpr {
    match query.op {
        Op::All => PlanExpr::All,
        Op::None => PlanExpr::None,
        Op::And => PlanExpr::AllOf {
            grams: public_grams(query.grams, fold, edges),
            needs: Vec::new(),
            children: public_children(query.sub, fold, edges),
        },
        Op::Or => PlanExpr::AnyOf {
            grams: public_grams(query.grams, fold, edges),
            needs: Vec::new(),
            children: public_children(query.sub, fold, edges),
        },
    }
}

fn public_grams(grams: StringSet, fold: bool, edges: Option<&[u8]>) -> Vec<GramNeedle> {
    grams
        .into_vec()
        .into_iter()
        .map(|gram| needle_for(&gram, fold, edges))
        .collect()
}

fn public_children(children: Vec<Query>, fold: bool, edges: Option<&[u8]>) -> Vec<PlanExpr> {
    children
        .into_iter()
        .map(|query| into_public_expr(query, fold, edges))
        .collect()
}

fn needle_for(gram: &Gram, fold: bool, edges: Option<&[u8]>) -> GramNeedle {
    let raw = GramKey(HashKey::UNKEYED.hash_bytes(gram.as_bytes()));
    let keys = if !fold || !gram.as_bytes().iter().any(u8::is_ascii_alphabetic) {
        vec![raw]
    } else {
        vec![
            raw,
            GramKey(HashKey::UNKEYED.folded().hash_bytes(gram.as_bytes())),
        ]
    };
    if let Some(needle) = edge_needle(gram, edges, &keys) {
        return needle;
    }
    if keys.len() == 1 {
        GramNeedle::Key(raw)
    } else {
        GramNeedle::AnyKey(keys)
    }
}

/// Word-edge needle for grams pinned to a word-bounded literal's edges
fn edge_needle(gram: &Gram, edges: Option<&[u8]>, keys: &[GramKey]) -> Option<GramNeedle> {
    let literal = edges?;
    let starts = literal.starts_with(gram.as_bytes());
    let ends = literal.ends_with(gram.as_bytes());
    (starts || ends).then(|| GramNeedle::AtWordEdge {
        keys: keys.to_vec(),
        starts,
        ends,
        whole: gram.as_bytes() == literal,
    })
}

/// The literal of a whole-pattern `\b literal \b` shape whose word-byte
/// edges make gram occurrences at the literal's edges word-bounded
fn word_edged_literal(hir: &Hir) -> Option<&[u8]> {
    let HirKind::Concat(subs) = hir.kind() else {
        return None;
    };
    let [first, mid, last] = subs.as_slice() else {
        return None;
    };
    if !is_word_look(first) || !is_word_look(last) {
        return None;
    }
    let HirKind::Literal(lit) = unwrap_captures(mid).kind() else {
        return None;
    };
    let (head, tail) = (*lit.0.first()?, *lit.0.last()?);
    (is_word_byte(head) && is_word_byte(tail)).then_some(&lit.0)
}

fn is_word_look(hir: &Hir) -> bool {
    matches!(
        hir.kind(),
        HirKind::Look(Look::WordAscii | Look::WordUnicode)
    )
}

fn unwrap_captures(hir: &Hir) -> &Hir {
    match hir.kind() {
        HirKind::Capture(capture) => unwrap_captures(&capture.sub),
        _ => hir,
    }
}
