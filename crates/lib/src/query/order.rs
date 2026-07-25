//! The two orders the analysis sorts byte strings in.
//!
//! Prefix order is ordinary lexicographic, grouping shared leading bytes.
//! Suffix order compares from the last byte back, grouping shared trailing
//! bytes. Both are total on distinct byte strings, and both compare the bytes
//! an edge truncation keeps before any byte it drops, so truncating a sorted
//! set leaves it sorted.

use core::cmp::Ordering;

use sngram_types::Gram;

/// Sort order for [`super::strings::StringSet::clean`] and friends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Ordinary lexicographic order; groups shared leading bytes.
    Prefix,
    /// Compared from the last byte back; groups shared trailing bytes.
    Suffix,
}

/// What is known about a set's order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Shape {
    /// Nothing known.
    #[default]
    Loose,
    /// Sorted in this order, duplicates possible.
    Sorted(Order),
    /// Sorted in this order with no duplicates.
    Clean(Order),
}

impl Shape {
    /// Whether the items are known to be sorted in `order`.
    pub const fn sorted_in(self, order: Order) -> bool {
        matches!(self, Self::Sorted(o) | Self::Clean(o) if o as u8 == order as u8)
    }

    /// Whether the items are known to be sorted in `order` and unique.
    pub const fn clean_in(self, order: Order) -> bool {
        matches!(self, Self::Clean(o) if o as u8 == order as u8)
    }

    /// Whether the items are known to be unique, whatever the order.
    pub const fn is_clean(self) -> bool {
        matches!(self, Self::Clean(_))
    }
}

/// Compare two strings in `order`.
pub fn cmp_in(order: Order, a: &Gram, b: &Gram) -> Ordering {
    match order {
        Order::Prefix => a.cmp(b),
        Order::Suffix => cmp_suffix(a.as_bytes(), b.as_bytes()),
    }
}

/// Compare two strings from their last byte back, then by length: the order
/// that groups shared suffixes adjacently for deduplication and truncation.
fn cmp_suffix(a: &[u8], b: &[u8]) -> Ordering {
    let mut ia = a.len();
    let mut ib = b.len();
    while ia > 0 && ib > 0 {
        ia -= 1;
        ib -= 1;
        match a[ia].cmp(&b[ib]) {
            Ordering::Equal => {},
            other => return other,
        }
    }
    a.len().cmp(&b.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gram(bytes: &[u8]) -> Gram {
        Gram::from(bytes)
    }

    #[test]
    fn suffix_order_reads_from_the_tail_then_the_length() {
        assert_eq!(
            cmp_in(Order::Suffix, &gram(b"aac"), &gram(b"abc")),
            Ordering::Less
        );
        assert_eq!(
            cmp_in(Order::Suffix, &gram(b"bc"), &gram(b"abc")),
            Ordering::Less
        );
        assert_eq!(
            cmp_in(Order::Suffix, &gram(b"abc"), &gram(b"abc")),
            Ordering::Equal
        );
    }

    #[test]
    fn truncation_keeps_both_orders_sorted() {
        let words: [&[u8]; 6] = [b"a", b"ab", b"abc", b"a\0c", b"zabc", b"yab"];
        for order in [Order::Prefix, Order::Suffix] {
            let mut sorted: Vec<Gram> = words.iter().map(|w| gram(w)).collect();
            sorted.sort_by(|a, b| cmp_in(order, a, b));
            for keep in 1..5 {
                let cut: Vec<Gram> = sorted.iter().map(|s| truncate(s, order, keep)).collect();
                assert!(
                    cut.windows(2)
                        .all(|pair| cmp_in(order, &pair[0], &pair[1]).is_le()),
                    "{order:?} keep {keep}"
                );
            }
        }
    }

    fn truncate(s: &Gram, order: Order, keep: usize) -> Gram {
        if s.len() <= keep {
            return s.clone();
        }
        match order {
            Order::Prefix => gram(&s[..keep]),
            Order::Suffix => gram(&s[s.len() - keep..]),
        }
    }
}
