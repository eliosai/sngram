//! The summary computed for each subexpression during analysis.
//!
//! `RegexpInfo` is the 5-tuple of Russ Cox's analysis: whether the
//! subexpression can match empty, its exact string set when finite, the
//! possible match prefixes and suffixes, and a [`Query`] already committed
//! for everything in between. Combining rules live in `combine.rs`.

use super::query::Query;
use super::strings::StringSet;

/// What analysis knows about one subexpression.
#[derive(Debug, Clone)]
pub struct RegexpInfo {
    /// Whether the subexpression matches the empty string.
    pub can_empty: bool,
    /// The exact, finite set of matching strings, when one exists.
    /// `None` means use [`Self::prefix`] and [`Self::suffix`] instead.
    pub exact: Option<StringSet>,
    /// Possible match prefixes (used only when `exact` is `None`).
    pub prefix: StringSet,
    /// Possible match suffixes (used only when `exact` is `None`).
    pub suffix: StringSet,
    /// A query every match must satisfy, beyond prefix/suffix.
    pub match_: Query,
}

impl RegexpInfo {
    /// A bare frame combining rules fill in; matches nothing until set.
    pub const fn blank() -> Self {
        Self {
            can_empty: false,
            exact: None,
            prefix: StringSet::new(),
            suffix: StringSet::new(),
            match_: Query::all(),
        }
    }

    /// Describes a regexp matching any string: the worst case, no constraint.
    pub fn any_match() -> Self {
        Self {
            can_empty: true,
            exact: None,
            prefix: empty_member(),
            suffix: empty_member(),
            match_: Query::all(),
        }
    }

    /// Describes a regexp matching any single character.
    pub fn any_char() -> Self {
        Self {
            can_empty: false,
            exact: None,
            prefix: empty_member(),
            suffix: empty_member(),
            match_: Query::all(),
        }
    }

    /// Describes a regexp matching no string at all.
    pub const fn no_match() -> Self {
        Self {
            can_empty: false,
            exact: None,
            prefix: StringSet::new(),
            suffix: StringSet::new(),
            match_: Query::none(),
        }
    }

    /// Describes a regexp matching only the empty string.
    pub fn empty_string() -> Self {
        Self {
            can_empty: true,
            exact: Some(StringSet::of(Vec::new())),
            prefix: StringSet::new(),
            suffix: StringSet::new(),
            match_: Query::all(),
        }
    }

    /// Describes a literal byte string.
    pub fn literal(bytes: &[u8]) -> Self {
        if bytes.is_empty() {
            return Self::empty_string();
        }
        Self {
            can_empty: false,
            exact: Some(StringSet::of(bytes.to_vec())),
            prefix: StringSet::new(),
            suffix: StringSet::new(),
            match_: Query::all(),
        }
    }
}

/// A set holding only the empty string, the identity for prefix/suffix cross
/// products (`"" + s == s`).
fn empty_member() -> StringSet {
    StringSet::of(Vec::new())
}
