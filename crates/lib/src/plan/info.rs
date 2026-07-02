//! The summary computed for each subexpression during analysis.
//!
//! `RegexpInfo` is the 5-tuple of Russ Cox's analysis: whether the
//! subexpression can match empty, its exact string set when finite, the
//! possible match prefixes and suffixes, and a [`Query`] already committed
//! for everything in between. Combining rules live in `combine.rs`.

use super::query::Query;
use super::strings::StringSet;

use crate::gram::Gram;

/// What analysis knows about one subexpression.
#[derive(Debug, Clone)]
pub struct RegexpInfo {
    /// Whether the subexpression matches the empty string.
    pub can_empty: bool,
    /// The exact, finite set of matching strings, when one exists.
    /// `None` means use [`Self::prefix`] and [`Self::suffix`] instead.
    pub exact: Option<StringSet>,
    /// Match prefixes (used only when `exact` is `None`). Exhaustive: every
    /// match STARTS with some member; a `""` member means the first byte is
    /// unknown. Look-around pruning reads first bytes from this, so dropping
    /// members is unsound.
    pub prefix: StringSet,
    /// Match suffixes (used only when `exact` is `None`). Exhaustive: every
    /// match ENDS with some member; a `""` member means the last byte is
    /// unknown. Look-around pruning reads last bytes from this, so dropping
    /// members is unsound.
    pub suffix: StringSet,
    /// When `Some(E)`, this info is a *pure* one-or-more repetition `E+` of the
    /// small exact set `E` (both edges an open `E`-run), freshly produced by
    /// [`super::analyze::demote_plus`]. It carries no constraint of its own —
    /// `prefix`/`suffix` already equal `E` and stay exhaustive — but lets the
    /// enclosing [`Analyzer::concat`] tighten the seam where the run abuts a
    /// neighbour (see `plus_prefix`/`plus_suffix`). It is a transient hint: the
    /// concat that consumes it bakes the tightened window into `prefix`/`suffix`
    /// and leaves the result's `plus_base` `None`, so every other combinator
    /// (alternation, a further concat, `blank`) clears it by construction.
    pub plus_base: Option<StringSet>,
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
            plus_base: None,
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
            plus_base: None,
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
            plus_base: None,
            match_: Query::none(),
        }
    }

    /// Describes a regexp matching only the empty string.
    pub fn empty_string() -> Self {
        Self {
            can_empty: true,
            exact: Some(StringSet::of(Gram::empty())),
            prefix: StringSet::new(),
            suffix: StringSet::new(),
            plus_base: None,
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
            exact: Some(StringSet::of(Gram::from(bytes))),
            prefix: StringSet::new(),
            suffix: StringSet::new(),
            plus_base: None,
            match_: Query::all(),
        }
    }
}

/// A set holding only the empty string, the identity for prefix/suffix cross
/// products (`"" + s == s`) and the "boundary byte unknown" sentinel for
/// look-around pruning, distinct from `can_empty`.
fn empty_member() -> StringSet {
    StringSet::of(Gram::empty())
}
