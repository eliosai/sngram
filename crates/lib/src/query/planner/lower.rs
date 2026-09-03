use crate::hashing::HashKey;
use crate::query::gram::Gram;
use crate::{GramKey, GramNeedle, PlanExpr};
use regex_syntax::hir::{Hir, HirKind, Look};

use crate::query::{
    algebra::{Op, Query},
    analyze::is_word_byte,
    strings::StringSet,
};

/// Lower an analyzed query into its public expression
pub fn query(query: Query, fold: bool, hir: &Hir) -> PlanExpr {
    let edges = if fold {
        EdgeShapes::default()
    } else {
        EdgeShapes::from_hir(hir)
    };
    into_public_expr(query, fold, &edges)
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

/// Literal and anchors of a whole-pattern line-anchored shape
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

fn split_start_anchor(subs: &[Hir]) -> (bool, &[Hir]) {
    match subs.split_first() {
        Some((edge, rest)) if is_line_start_look(edge) => (true, rest),
        _ => (false, subs),
    }
}

fn split_end_anchor(subs: &[Hir]) -> (bool, &[Hir]) {
    match subs.split_last() {
        Some((edge, rest)) if is_line_end_look(edge) => (true, rest),
        _ => (false, subs),
    }
}

fn is_line_start_look(hir: &Hir) -> bool {
    matches!(hir.kind(), HirKind::Look(Look::Start | Look::StartLF))
}

fn is_line_end_look(hir: &Hir) -> bool {
    matches!(hir.kind(), HirKind::Look(Look::End | Look::EndLF))
}

/// Literal in a whole-pattern word-bounded shape
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
