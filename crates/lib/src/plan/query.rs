//! The boolean gram query and its simplifying algebra.
//!
//! A [`Query`] is a conservative matching machine: it matches everything the
//! source regex matches, and usually more. The algebra (`and`, `or`) keeps
//! the tree small by absorbing identities, merging same-op nodes, factoring
//! out shared grams, and dropping clauses that another clause already
//! implies. Ported from Google codesearch `Query`.

use core::cmp::Ordering;

use super::strings::{Order, StringSet};

/// The operator at a query node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Matches every document; the index cannot constrain this clause.
    All,
    /// Matches no document; provably empty.
    None,
    /// Every gram and every sub-query must be present.
    And,
    /// At least one gram or sub-query must be present.
    Or,
}

/// A conservative boolean query over gram presence.
///
/// For [`Op::And`]/[`Op::Or`], `grams` holds atomic gram leaves (so single
/// grams need no wrapper node) and `sub` holds nested queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub op: Op,
    pub grams: StringSet,
    pub sub: Vec<Self>,
}

impl Query {
    /// The always-true query.
    #[must_use]
    pub const fn all() -> Self {
        Self::leaf(Op::All)
    }

    /// The always-false query.
    #[must_use]
    pub const fn none() -> Self {
        Self::leaf(Op::None)
    }

    /// An `op` node carrying exactly the gram set `grams`.
    #[must_use]
    pub const fn grams(op: Op, grams: StringSet) -> Self {
        Self {
            op,
            grams,
            sub: Vec::new(),
        }
    }

    const fn leaf(op: Op) -> Self {
        Self {
            op,
            grams: StringSet::new(),
            sub: Vec::new(),
        }
    }

    /// Whether this node is a single gram, the atom that merges into any
    /// same-op parent without nesting.
    fn is_atom(&self) -> bool {
        self.grams.len() == 1 && self.sub.is_empty()
    }

    /// Total grams in this query tree: the cost cloning it replicates.
    pub fn weight(&self) -> usize {
        self.grams.len() + self.sub.iter().map(Self::weight).sum::<usize>()
    }

    fn is(&self, op: Op) -> bool {
        self.op == op
    }

    /// `self AND other`, simplified.
    #[must_use]
    pub fn and(self, other: Self) -> Self {
        self.and_or(other, Op::And)
    }

    /// `self OR other`, simplified.
    #[must_use]
    pub fn or(self, other: Self) -> Self {
        self.and_or(other, Op::Or)
    }

    /// `self` combined with `other` under `op`, working to avoid building
    /// needlessly complicated structures. Reuses both operands' storage; no
    /// node is cloned.
    #[must_use]
    pub fn and_or(self, other: Self, op: Op) -> Self {
        let q = unwrap_single(self);
        let r = unwrap_single(other);
        // q ⇒ r: under AND keep the stronger q, under OR keep the weaker r.
        if q.implies(&r) {
            return if op == Op::And { q } else { r };
        }
        if r.implies(&q) {
            return if op == Op::And { r } else { q };
        }
        merge(q, r, op)
    }

    /// Whether `self` implies `other`: every document matching `self` also
    /// matches `other`. False negatives are allowed (they only cost size).
    #[must_use]
    pub fn implies(&self, other: &Self) -> bool {
        match (self.op, other.op) {
            (Op::None, _) | (_, Op::All) => true,
            (Op::All, _) | (_, Op::None) => false,
            _ => self.implies_inner(other),
        }
    }

    fn implies_inner(&self, other: &Self) -> bool {
        if self.is(Op::And) || (self.is(Op::Or) && self.is_atom()) {
            return grams_imply(&self.grams, other);
        }
        // An OR implies `other` when every alternative does: each sub-query,
        // and each gram on its own.
        self.is(Op::Or)
            && self.sub.iter().all(|q| q.implies(other))
            && self.grams.as_slice().iter().all(|g| gram_implies(g, other))
    }
}

/// A node `op{single sub}` carries no information beyond that sub-query.
fn unwrap_single(mut q: Query) -> Query {
    if q.grams.is_empty() && q.sub.len() == 1 {
        if let Some(only) = q.sub.pop() {
            return only;
        }
    }
    q
}

/// Merge two non-implying queries under `op`: fold a matching operator into
/// its partner, pair two atoms, attach to an operator node, else factor.
fn merge(q: Query, r: Query, op: Op) -> Query {
    let (q_atom, r_atom) = (q.is_atom(), r.is_atom());
    if q.is(op) && (r.is(op) || r_atom) {
        return absorb(q, r);
    }
    if r.is(op) && q_atom {
        return absorb(r, q);
    }
    if q_atom && r_atom {
        return pair(q, &r, op);
    }
    if q.is(op) {
        return attach(q, r);
    }
    if r.is(op) {
        return attach(r, q);
    }
    factor(q, r, op)
}

/// Fold `guest`'s grams and sub-queries into `host`, which already uses the op.
fn absorb(mut host: Query, mut guest: Query) -> Query {
    host.grams = host.grams.union(&guest.grams, Order::Prefix);
    host.sub.append(&mut guest.sub);
    host
}

/// Combine two single-gram atoms into one `op` node holding both grams.
fn pair(mut q: Query, r: &Query, op: Op) -> Query {
    q.op = op;
    q.grams = q.grams.union(&r.grams, Order::Prefix);
    q
}

/// Attach `child` as a sub-query of operator node `host`.
fn attach(mut host: Query, child: Query) -> Query {
    host.sub.push(child);
    host
}

/// Build an AND of ORs (or OR of ANDs) for `q` and `r`, first pulling any
/// grams common to both out into the opposite operator.
fn factor(mut q: Query, mut r: Query, op: Op) -> Query {
    let common = split_common(&mut q.grams, &mut r.grams);
    if common.is_empty() {
        return Query {
            op,
            grams: StringSet::new(),
            sub: vec![q, r],
        };
    }
    let inner = q.and_or(r, op);
    let other = invert(op);
    Query::grams(other, common).and_or(inner, other)
}

/// Remove the grams present in both sets and return them, leaving each input
/// with only its own grams. Both are [`Order::Prefix`]-cleaned, so one merge
/// walk finds the intersection.
#[allow(
    clippy::indexing_slicing,
    reason = "qi, ri stay below the lengths checked"
)]
fn split_common(q: &mut StringSet, r: &mut StringSet) -> StringSet {
    let qs = std::mem::take(q).into_vec();
    let rs = std::mem::take(r).into_vec();
    let (mut qi, mut ri) = (0, 0);
    let mut common = StringSet::new();
    while qi < qs.len() && ri < rs.len() {
        match qs[qi].cmp(&rs[ri]) {
            Ordering::Less => take_one(q, &qs, &mut qi),
            Ordering::Greater => take_one(r, &rs, &mut ri),
            Ordering::Equal => {
                common.push(qs[qi].clone());
                qi += 1;
                ri += 1;
            },
        }
    }
    drain_rest(q, &qs, qi);
    drain_rest(r, &rs, ri);
    common
}

#[allow(clippy::indexing_slicing, reason = "caller guards *i < src.len()")]
fn take_one(out: &mut StringSet, src: &[crate::gram::Gram], i: &mut usize) {
    out.push(src[*i].clone());
    *i += 1;
}

fn drain_rest(out: &mut StringSet, src: &[crate::gram::Gram], from: usize) {
    for s in src.iter().skip(from) {
        out.push(s.clone());
    }
}

const fn invert(op: Op) -> Op {
    match op {
        Op::And => Op::Or,
        _ => Op::And,
    }
}

/// Whether the conjunction of grams `t` implies query `q`.
fn grams_imply(t: &StringSet, q: &Query) -> bool {
    match q.op {
        Op::Or => q.sub.iter().any(|qq| grams_imply(t, qq)) || any_gram_in(t, &q.grams),
        Op::And => q.sub.iter().all(|qq| grams_imply(t, qq)) && q.grams.is_subset_of(t),
        _ => false,
    }
}

/// Whether any single gram of `t` already appears in `set`.
fn any_gram_in(t: &StringSet, set: &StringSet) -> bool {
    t.as_slice()
        .iter()
        .any(|g| StringSet::of(g.clone()).is_subset_of(set))
}

/// Whether the presence of the single gram `g` implies query `q`.
fn gram_implies(g: &crate::gram::Gram, q: &Query) -> bool {
    grams_imply(&StringSet::of(g.clone()), q)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gram(b: &[u8]) -> Query {
        Query::grams(Op::And, StringSet::of(crate::gram::Gram::from(b)))
    }

    fn cleaned(items: &[&[u8]]) -> StringSet {
        let mut s = StringSet::new();
        for it in items {
            s.push(crate::gram::Gram::from(*it));
        }
        s.clean(Order::Prefix);
        s
    }

    #[test]
    fn test_and_absorbs_all_identity() {
        let actual = gram(b"abc").and(Query::all());
        assert_eq!(actual, gram(b"abc"));
    }

    #[test]
    fn test_or_absorbs_none_identity() {
        let actual = gram(b"abc").or(Query::none());
        assert_eq!(actual, gram(b"abc"));
    }

    #[test]
    fn test_and_with_none_is_none() {
        let actual = gram(b"abc").and(Query::none());
        assert_eq!(actual.op, Op::None);
    }

    #[test]
    fn test_or_with_all_is_all() {
        let actual = gram(b"abc").or(Query::all());
        assert_eq!(actual.op, Op::All);
    }

    #[test]
    fn test_and_merges_atoms_into_one_node() {
        let actual = gram(b"abc").and(gram(b"def"));
        assert_eq!(actual.op, Op::And);
        assert_eq!(actual.grams, cleaned(&[b"abc", b"def"]));
        assert!(actual.sub.is_empty());
    }

    #[test]
    fn test_duplicate_and_collapses() {
        // abc AND abc ≡ abc (implication both ways).
        let actual = gram(b"abc").and(gram(b"abc"));
        assert_eq!(actual, gram(b"abc"));
    }

    #[test]
    fn test_or_of_same_atom_collapses() {
        let actual = gram(b"abc").or(gram(b"abc"));
        assert_eq!(actual, gram(b"abc"));
    }

    #[test]
    fn test_factor_common_gram_out_of_ors() {
        // (abc|def) AND (abc|ghi) factors to abc | (def AND ghi).
        let left = gram(b"abc").or(gram(b"def"));
        let right = gram(b"abc").or(gram(b"ghi"));
        let actual = left.and(right);
        // Common "abc" implies each side, so the AND collapses to "abc".
        assert!(grams_imply(
            &StringSet::of(crate::gram::Gram::from(&b"abc"[..])),
            &actual
        ));
    }

    #[test]
    fn test_implies_subset_or() {
        let narrow = Query::grams(Op::Or, cleaned(&[b"abc"]));
        let wide = Query::grams(Op::Or, cleaned(&[b"abc", b"def"]));
        assert!(narrow.implies(&wide));
        assert!(!wide.implies(&narrow));
    }
}
