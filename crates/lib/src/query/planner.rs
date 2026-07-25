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
            EdgeShapes::default()
        } else {
            EdgeShapes::from_hir(hir)
        };
        with_root_needs(
            into_public_expr(analyzer.plan(hir), ctx.fold, &edges),
            RootNeeds::from_hir(hir),
        )
    }
}

/// Whole-pattern shapes that pin gram occurrences to word or line edges
#[derive(Default)]
struct EdgeShapes<'a> {
    word: Option<&'a [u8]>,
    line: Option<LineAnchored<'a>>,
}

impl<'a> EdgeShapes<'a> {
    fn from_hir(hir: &'a Hir) -> Self {
        Self {
            word: word_edged_literal(hir),
            line: line_anchored_literal(hir),
        }
    }
}

/// A literal that whole-pattern line anchors pin to the sides they bind
struct LineAnchored<'a> {
    literal: &'a [u8],
    starts: bool,
    ends: bool,
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

fn into_public_expr(query: Query, fold: bool, edges: &EdgeShapes<'_>) -> PlanExpr {
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

fn public_grams(grams: StringSet, fold: bool, edges: &EdgeShapes<'_>) -> Vec<GramNeedle> {
    grams
        .into_vec()
        .into_iter()
        .map(|gram| needle_for(&gram, fold, edges))
        .collect()
}

fn public_children(children: Vec<Query>, fold: bool, edges: &EdgeShapes<'_>) -> Vec<PlanExpr> {
    children
        .into_iter()
        .map(|query| into_public_expr(query, fold, edges))
        .collect()
}

fn needle_for(gram: &Gram, fold: bool, edges: &EdgeShapes<'_>) -> GramNeedle {
    let raw = GramKey(HashKey::UNKEYED.hash_bytes(gram.as_bytes()));
    let keys = if !fold || !gram.as_bytes().iter().any(u8::is_ascii_alphabetic) {
        vec![raw]
    } else {
        vec![
            raw,
            GramKey(HashKey::UNKEYED.folded().hash_bytes(gram.as_bytes())),
        ]
    };
    if let Some(needle) = edge_needle(gram, edges.word, &keys) {
        return needle;
    }
    if let Some(needle) = line_edge_needle(gram, edges.line.as_ref(), &keys) {
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

/// Line-edge needle for grams pinned to a line-anchored literal's edges
fn line_edge_needle(
    gram: &Gram,
    anchors: Option<&LineAnchored<'_>>,
    keys: &[GramKey],
) -> Option<GramNeedle> {
    let anchors = anchors?;
    let starts = anchors.starts && anchors.literal.starts_with(gram.as_bytes());
    let ends = anchors.ends && anchors.literal.ends_with(gram.as_bytes());
    (starts || ends).then(|| GramNeedle::AtLineEdge {
        keys: keys.to_vec(),
        starts,
        ends,
    })
}

/// The literal of a whole-pattern `^ literal $` shape, with the anchors it
/// carries. A match places the literal's leading gram at a line start and its
/// trailing gram at a line end, so those occurrences are provably line-edged
fn line_anchored_literal(hir: &Hir) -> Option<LineAnchored<'_>> {
    let HirKind::Concat(subs) = hir.kind() else {
        return None;
    };
    let (starts, rest) = split_start_anchor(subs);
    let (ends, rest) = split_end_anchor(rest);
    let [mid] = rest else {
        return None;
    };
    let HirKind::Literal(lit) = unwrap_captures(mid).kind() else {
        return None;
    };
    ((starts || ends) && !lit.0.is_empty()).then_some(LineAnchored {
        literal: &lit.0,
        starts,
        ends,
    })
}

/// Peel a leading line-start anchor, reporting whether it was there
fn split_start_anchor(subs: &[Hir]) -> (bool, &[Hir]) {
    match subs.split_first() {
        Some((edge, rest)) if is_line_start_look(edge) => (true, rest),
        _ => (false, subs),
    }
}

/// Peel a trailing line-end anchor, reporting whether it was there
fn split_end_anchor(subs: &[Hir]) -> (bool, &[Hir]) {
    match subs.split_last() {
        Some((edge, rest)) if is_line_end_look(edge) => (true, rest),
        _ => (false, subs),
    }
}

/// `^` and `\A`, whose matches always sit at a stored line start. CRLF anchors
/// are excluded: they also match before a `\r` the stored bits do not record
fn is_line_start_look(hir: &Hir) -> bool {
    matches!(hir.kind(), HirKind::Look(Look::Start | Look::StartLF))
}

/// `$` and `\z`, whose matches always sit at a stored line end
fn is_line_end_look(hir: &Hir) -> bool {
    matches!(hir.kind(), HirKind::Look(Look::End | Look::EndLF))
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

#[cfg(test)]
mod tests {
    use sngram_types::{GramNeedle, PlanExpr, WeightTable};

    use crate::query::query;

    fn table() -> WeightTable {
        WeightTable::from_weight_fn(|c1, c2| crc32fast::hash(&[c1, c2]))
    }

    /// The line-edge sides demanded across every needle of a plan's root
    fn line_edges(re: &str) -> Vec<(bool, bool)> {
        let plan = query(&table(), re).expect("pattern plans");
        let PlanExpr::AllOf { grams, .. } = plan.root() else {
            return Vec::new();
        };
        grams
            .iter()
            .filter_map(|needle| match needle {
                GramNeedle::AtLineEdge { starts, ends, .. } => Some((*starts, *ends)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn fully_anchored_literal_pins_both_of_its_edges() {
        let edges = line_edges("^kfree_skb;$");
        assert!(edges.iter().any(|&(starts, _)| starts), "{edges:?}");
        assert!(edges.iter().any(|&(_, ends)| ends), "{edges:?}");
    }

    #[test]
    fn a_gram_spanning_the_whole_literal_pins_both_sides_at_once() {
        let edges = line_edges("^done$");
        assert!(edges.contains(&(true, true)), "{edges:?}");
    }

    #[test]
    fn one_sided_anchors_pin_only_their_own_side() {
        assert!(line_edges("^kfree_skb").iter().all(|&(_, ends)| !ends));
        assert!(line_edges("kfree_skb$").iter().all(|&(starts, _)| !starts));
    }

    #[test]
    fn text_anchors_pin_line_edges_too() {
        assert!(!line_edges(r"\Akfree_skb\z").is_empty());
    }

    #[test]
    fn crlf_anchors_stay_unpinned() {
        assert!(
            line_edges("(?R:^kfree_skb$)").is_empty(),
            "a CRLF `$` also matches before a `\\r`, which the stored bits do not record"
        );
    }

    #[test]
    fn unanchored_and_nonliteral_shapes_stay_unpinned() {
        assert!(line_edges("kfree_skb").is_empty());
        assert!(line_edges(r"^[ \t]*kfree_skb").is_empty());
        assert!(line_edges("(?i:^kfree_skb$)").is_empty());
    }
}
