use crate::query::gram::Gram;

use super::StringSet;
use crate::query::order::{Order, Shape, cmp_in};

impl StringSet {
    /// Union with `other`, cleaned in `order`, reusing this set's storage
    #[must_use]
    pub fn union(mut self, other: &Self, order: Order) -> Self {
        if self.shape.sorted_in(order) && other.shape.sorted_in(order) {
            self.merge_from(&other.items, order);
            self.shape = Shape::Sorted(order);
        } else {
            self.items.extend_from_slice(&other.items);
            self.shape = Shape::Loose;
        }
        self.clean(order);
        self
    }

    /// Merge an already-ordered `other` into this already-ordered set
    fn merge_from(&mut self, other: &[Gram], order: Order) {
        let held = self.items.len();
        self.items.resize(held + other.len(), Gram::empty());
        let (mut i, mut j, mut k) = (held, other.len(), held + other.len());
        while j > 0 {
            k -= 1;
            if i > 0 && cmp_in(order, &self.items[i - 1], &other[j - 1]).is_gt() {
                i -= 1;
                self.items.swap(k, i);
            } else {
                j -= 1;
                self.items[k] = other[j].clone();
            }
        }
    }

    /// Whether `s` is a member
    #[must_use]
    pub fn contains(&self, s: &Gram) -> bool {
        if self.shape.sorted_in(Order::Prefix) {
            let at = self.items.partition_point(|item| item < s);
            return self.items.get(at).is_some_and(|item| item == s);
        }
        self.items
            .iter()
            .take_while(|item| *item <= s)
            .any(|item| item == s)
    }

    /// Every string from `self` concatenated with every string from `other`
    #[must_use]
    pub fn cross(&self, other: &Self, order: Order) -> Self {
        let mut out = Vec::with_capacity(self.items.len() * other.items.len());
        for a in &self.items {
            for b in &other.items {
                out.push(Gram::concat(a.as_bytes(), b.as_bytes()));
            }
        }
        let mut set = Self {
            items: out,
            shape: self.cross_shape(other),
        };
        set.clean(order);
        set
    }

    /// The known size of [`Self::cross`] with `other`
    #[must_use]
    pub fn cross_len(&self, other: &Self) -> Option<usize> {
        let countable = self.shape.is_clean() && other.shape.is_clean() && self.uniform_len();
        countable.then(|| self.items.len().saturating_mul(other.items.len()))
    }

    fn cross_shape(&self, other: &Self) -> Shape {
        let ordered = self.shape.clean_in(Order::Prefix)
            && other.shape.clean_in(Order::Prefix)
            && self.uniform_len();
        if ordered {
            Shape::Clean(Order::Prefix)
        } else {
            Shape::Loose
        }
    }

    fn uniform_len(&self) -> bool {
        let mut lens = self.items.iter().map(|g| g.len());
        let Some(first) = lens.next() else {
            return true;
        };
        lens.all(|len| len == first)
    }

    /// Whether every string in `self` also appears in `other`
    #[must_use]
    pub fn is_subset_of(&self, other: &[Gram]) -> bool {
        let mut j = 0;
        for s in &self.items {
            while j < other.len() && other[j] < *s {
                j += 1;
            }
            if j >= other.len() || other[j] != *s {
                return false;
            }
        }
        true
    }
}
