//! Set algebra over posting lists kept sorted by document ordinal.

use super::{BLOCK_BITS, Posting};

/// List-length ratio past which intersection gallops through the longer side
const GALLOP_RATIO: usize = 16;

/// Borrow each owned list as a slice a union can walk
pub fn borrowed(lists: &[Vec<Posting>]) -> Vec<&[Posting]> {
    lists.iter().map(Vec::as_slice).collect()
}

/// Merge sorted posting lists in one pass, joining masks on equal ordinals
pub fn union_all(lists: &[&[Posting]]) -> Vec<Posting> {
    match lists {
        [] => Vec::new(),
        [only] => only.to_vec(),
        [left, right] => union_pair(left, right),
        _ => union_many(lists),
    }
}

/// Merge sorted ordinals, dropping the duplicates the two sides share
pub fn union_sorted(left: Vec<usize>, right: Vec<usize>) -> Vec<usize> {
    let mut out = Vec::with_capacity(left.len() + right.len());
    let mut i = 0;
    let mut j = 0;
    while i < left.len() && j < right.len() {
        match left[i].cmp(&right[j]) {
            std::cmp::Ordering::Less => {
                out.push(left[i]);
                i += 1;
            },
            std::cmp::Ordering::Greater => {
                out.push(right[j]);
                j += 1;
            },
            std::cmp::Ordering::Equal => {
                out.push(left[i]);
                i += 1;
                j += 1;
            },
        }
    }
    out.extend_from_slice(&left[i..]);
    out.extend_from_slice(&right[j..]);
    out
}

/// Intersect every list, shortest first, so an empty result stops the walk
pub fn intersect_all(
    mut lists: Vec<std::rc::Rc<Vec<Posting>>>,
    all_text: impl FnOnce() -> Vec<Posting>,
) -> Vec<Posting> {
    lists.sort_by_key(|list| list.len());
    let mut iter = lists.into_iter();
    let Some(first) = iter.next() else {
        return all_text();
    };
    let mut acc = first.as_ref().clone();
    for list in iter {
        acc = intersect_postings(&acc, &list);
        if acc.is_empty() {
            break;
        }
    }
    acc
}

fn union_pair(left: &[Posting], right: &[Posting]) -> Vec<Posting> {
    let mut out = Vec::with_capacity(left.len() + right.len());
    let mut i = 0;
    let mut j = 0;
    while i < left.len() && j < right.len() {
        match left[i].ord.cmp(&right[j].ord) {
            std::cmp::Ordering::Less => {
                out.push(left[i]);
                i += 1;
            },
            std::cmp::Ordering::Greater => {
                out.push(right[j]);
                j += 1;
            },
            std::cmp::Ordering::Equal => {
                out.push(Posting {
                    ord: left[i].ord,
                    mask: left[i].mask | right[j].mask,
                });
                i += 1;
                j += 1;
            },
        }
    }
    out.extend_from_slice(&left[i..]);
    out.extend_from_slice(&right[j..]);
    out
}

/// Take the lowest ordinal off every list that holds it, until one list is left
fn union_many(lists: &[&[Posting]]) -> Vec<Posting> {
    let mut heads: Vec<&[Posting]> = lists
        .iter()
        .copied()
        .filter(|list| !list.is_empty())
        .collect();
    let mut out = Vec::with_capacity(lists.iter().map(|list| list.len()).sum());
    while heads.len() > 1 {
        let ord = heads.iter().map(|list| list[0].ord).min().unwrap_or(0);
        out.push(Posting {
            ord,
            mask: take_ord(&mut heads, ord),
        });
    }
    if let Some(rest) = heads.first() {
        out.extend_from_slice(rest);
    }
    out
}

/// Drop the head of every list at this ordinal and join the masks it carried
fn take_ord(heads: &mut Vec<&[Posting]>, ord: usize) -> u16 {
    let mut mask = 0u16;
    let mut at = 0usize;
    while at < heads.len() {
        if heads[at][0].ord != ord {
            at += 1;
            continue;
        }
        mask |= heads[at][0].mask;
        heads[at] = &heads[at][1..];
        if heads[at].is_empty() {
            heads.swap_remove(at);
        } else {
            at += 1;
        }
    }
    mask
}

/// Keep ordinals present in both lists whose block masks overlap
fn intersect_postings(left: &[Posting], right: &[Posting]) -> Vec<Posting> {
    let (short, long) = if left.len() <= right.len() {
        (left, right)
    } else {
        (right, left)
    };
    if short.len().saturating_mul(GALLOP_RATIO) < long.len() {
        return gallop_intersect(short, long);
    }
    let mut out = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < left.len() && j < right.len() {
        match left[i].ord.cmp(&right[j].ord) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                push_overlap(&mut out, left[i], right[j]);
                i += 1;
                j += 1;
            },
        }
    }
    out
}

/// Binary-search each short-list ordinal inside the remaining long tail
fn gallop_intersect(short: &[Posting], long: &[Posting]) -> Vec<Posting> {
    let mut out = Vec::new();
    let mut base = 0usize;
    for &posting in short {
        let tail = &long[base..];
        match tail.binary_search_by_key(&posting.ord, |p| p.ord) {
            Ok(at) => {
                push_overlap(&mut out, posting, tail[at]);
                base += at + 1;
            },
            Err(at) => base += at,
        }
        if base >= long.len() {
            break;
        }
    }
    out
}

fn push_overlap(out: &mut Vec<Posting>, a: Posting, b: Posting) {
    let mask = a.mask & b.mask;
    if mask & BLOCK_BITS != 0 {
        out.push(Posting { ord: a.ord, mask });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pairwise fold `union_all` replaced, kept as the equivalence oracle
    fn folded(lists: &[&[Posting]]) -> Vec<Posting> {
        let mut acc: Vec<Posting> = Vec::new();
        for list in lists {
            acc = union_pair(&acc, list);
        }
        acc
    }

    fn sample(seed: u64, ords: &[usize]) -> Vec<Posting> {
        ords.iter()
            .map(|&ord| Posting {
                ord,
                mask: 1 << ((seed as usize + ord) % 10),
            })
            .collect()
    }

    /// Deterministic pseudo-random ascending ordinals
    fn spread(seed: u64, len: usize, span: usize) -> Vec<usize> {
        let mut state = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let mut ords = Vec::new();
        let mut ord = 0usize;
        for _ in 0..len {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ord += 1 + (state >> 33) as usize % span;
            ords.push(ord);
        }
        ords
    }

    #[test]
    fn many_way_union_matches_the_pairwise_fold() {
        for seed in 0..40u64 {
            let lists: Vec<Vec<Posting>> = (0..(seed % 6 + 1))
                .map(|k| sample(seed + k, &spread(seed * 17 + k, 20, 5)))
                .collect();
            let borrowed: Vec<&[Posting]> = lists.iter().map(Vec::as_slice).collect();

            assert_eq!(
                union_all(&borrowed),
                folded(&borrowed),
                "seed {seed} diverged"
            );
        }
    }

    fn masked(pairs: &[(usize, u16)]) -> Vec<Posting> {
        pairs
            .iter()
            .map(|&(ord, mask)| Posting { ord, mask })
            .collect()
    }

    #[test]
    fn many_way_union_joins_masks_and_stays_sorted() {
        let a = masked(&[(1, 0b001), (5, 0b010)]);
        let b = masked(&[(1, 0b100), (3, 0b001)]);
        let c = masked(&[(5, 0b001), (9, 0b010)]);

        assert_eq!(
            union_all(&[&a, &b, &c]),
            masked(&[(1, 0b101), (3, 0b001), (5, 0b011), (9, 0b010)])
        );
    }

    #[test]
    fn many_way_union_ignores_empty_lists() {
        let a = masked(&[(2, 1)]);
        let empty: Vec<Posting> = Vec::new();

        assert_eq!(union_all(&[&empty, &a, &empty, &empty]), a);
        assert_eq!(union_all(&[&empty, &empty, &empty]), Vec::new());
    }
}
