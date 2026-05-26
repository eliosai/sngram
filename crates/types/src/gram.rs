//! Gram types for index and query operations.

use core::fmt;

/// Borrowed n-gram from indexed content.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct IndexGram<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> IndexGram<'a> {
    #[must_use]
    pub const fn new(bytes: &'a [u8], offset: usize) -> Self {
        Self { bytes, offset }
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &'a [u8] { self.bytes }

    #[must_use]
    pub const fn offset(&self) -> usize { self.offset }

    #[must_use]
    pub const fn len(&self) -> usize { self.bytes.len() }

    #[must_use]
    pub const fn is_empty(&self) -> bool { self.bytes.is_empty() }
}

impl fmt::Debug for IndexGram<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "IndexGram({:?}@{})", String::from_utf8_lossy(self.bytes), self.offset)
    }
}

/// Owned n-gram from query decomposition.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct QueryGram {
    bytes: Vec<u8>,
    offset: usize,
}

impl QueryGram {
    #[must_use]
    pub const fn new(bytes: Vec<u8>, offset: usize) -> Self {
        Self { bytes, offset }
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] { &self.bytes }

    #[must_use]
    pub const fn offset(&self) -> usize { self.offset }

    #[must_use]
    pub fn len(&self) -> usize { self.bytes.len() }

    #[must_use]
    pub fn is_empty(&self) -> bool { self.bytes.is_empty() }
}

impl fmt::Debug for QueryGram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "QueryGram({:?}@{})", String::from_utf8_lossy(&self.bytes), self.offset)
    }
}

/// Collection from [`sngram::index`].
#[derive(Debug)]
pub struct IndexGrams<'a>(Vec<IndexGram<'a>>);

impl<'a> IndexGrams<'a> {
    #[must_use]
    pub const fn new(grams: Vec<IndexGram<'a>>) -> Self { Self(grams) }

    #[must_use]
    pub fn len(&self) -> usize { self.0.len() }

    #[must_use]
    pub fn is_empty(&self) -> bool { self.0.is_empty() }

    #[must_use]
    pub fn into_inner(self) -> Vec<IndexGram<'a>> { self.0 }

    pub fn iter(&self) -> core::slice::Iter<'_, IndexGram<'a>> { self.0.iter() }

    pub fn hashes(&self) -> impl Iterator<Item = u64> + '_ {
        self.0.iter().map(|g| hash_bytes(g.bytes))
    }
}

impl<'a> IntoIterator for IndexGrams<'a> {
    type Item = IndexGram<'a>;
    type IntoIter = std::vec::IntoIter<IndexGram<'a>>;
    fn into_iter(self) -> Self::IntoIter { self.0.into_iter() }
}

impl<'a, 'b> IntoIterator for &'b IndexGrams<'a> {
    type Item = &'b IndexGram<'a>;
    type IntoIter = core::slice::Iter<'b, IndexGram<'a>>;
    fn into_iter(self) -> Self::IntoIter { self.0.iter() }
}

/// Collection from [`sngram::query`].
#[derive(Debug)]
pub struct QueryGrams(Vec<QueryGram>);

impl QueryGrams {
    #[must_use]
    pub const fn new(grams: Vec<QueryGram>) -> Self { Self(grams) }

    #[must_use]
    pub fn len(&self) -> usize { self.0.len() }

    #[must_use]
    pub fn is_empty(&self) -> bool { self.0.is_empty() }

    #[must_use]
    pub fn into_inner(self) -> Vec<QueryGram> { self.0 }

    pub fn iter(&self) -> core::slice::Iter<'_, QueryGram> { self.0.iter() }

    pub fn hashes(&self) -> impl Iterator<Item = u64> + '_ {
        self.0.iter().map(|g| hash_bytes(&g.bytes))
    }
}

impl IntoIterator for QueryGrams {
    type Item = QueryGram;
    type IntoIter = std::vec::IntoIter<QueryGram>;
    fn into_iter(self) -> Self::IntoIter { self.0.into_iter() }
}

impl<'a> IntoIterator for &'a QueryGrams {
    type Item = &'a QueryGram;
    type IntoIter = core::slice::Iter<'a, QueryGram>;
    fn into_iter(self) -> Self::IntoIter { self.0.iter() }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    use core::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    bytes.hash(&mut hasher);
    hasher.finish()
}
