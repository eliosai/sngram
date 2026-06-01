//! Folding a regex HIR bottom-up into a [`RegexpInfo`].
//!
//! Each HIR node maps to the constructor or combining rule that conservatively
//! describes it. Case-insensitivity needs no handling here: `regex-syntax`
//! expands `(?i)` into character classes during parsing, so concat-of-classes
//! reproduces the folded variant sets for free.

use regex_syntax::hir::{Class, Hir, HirKind, Repetition};

use sngram_types::WeightTable;

use super::info::RegexpInfo;
use super::strings::{Order, StringSet};

/// Flush the exact set once it holds more than this many strings. Sized so
/// three case-folded letters (2³ = 8 variants) trigger a flush, as codesearch.
pub const MAX_EXACT: usize = 7;
/// Upper bound on prefix and suffix set sizes.
pub const MAX_SET: usize = 20;
/// Bytes of prefix/suffix kept across a concat seam, matching codesearch.
///
/// Two is the minimum that still forms a gram with one byte from the other
/// side of the seam. The sparse advantage comes from covering each literal
/// (long, rare grams), not from widening seams: a longer context only flushes
/// suffix/prefix grams that the per-branch covering already implies, enlarging
/// the query without narrowing the candidate set. Every gram in the plan is a
/// sparse gram regardless, since [`crate::extract::cover_one`] produces them.
pub const BOUNDARY_CTX: usize = 2;
/// Character-class size past which we stop enumerating and over-approximate.
pub const MAX_CLASS: u64 = 100;

/// Folds a regex HIR into a conservative gram query using a weight table.
pub struct Analyzer<'a> {
    table: &'a WeightTable,
}

impl<'a> Analyzer<'a> {
    /// Bind an analyzer to the weight table used to cover literals.
    pub const fn new(table: &'a WeightTable) -> Self {
        Self { table }
    }

    /// The weight table this analyzer covers literals with.
    pub const fn table(&self) -> &WeightTable {
        self.table
    }

    /// Analyze `hir`, returning its conservative summary.
    pub fn analyze(&self, hir: &Hir) -> RegexpInfo {
        let mut info = match hir.kind() {
            HirKind::Empty | HirKind::Look(_) => return RegexpInfo::empty_string(),
            HirKind::Capture(c) => return self.analyze(&c.sub),
            HirKind::Concat(subs) => return self.fold(subs, Combine::Concat),
            HirKind::Alternation(subs) => return self.fold(subs, Combine::Alternate),
            HirKind::Repetition(rep) => return self.repetition(rep),
            HirKind::Literal(lit) => RegexpInfo::literal(&lit.0),
            HirKind::Class(cls) => class(cls),
        };
        self.simplify(&mut info, false);
        info
    }

    /// `x?`, `x*`, `x+`, `x{n,m}`: collapse to the conservative case.
    fn repetition(&self, rep: &Repetition) -> RegexpInfo {
        match (rep.min, rep.max) {
            (0, Some(1)) => self.alternate(self.analyze(&rep.sub), RegexpInfo::empty_string()),
            (0, _) => RegexpInfo::any_match(),
            _ => {
                let mut info = demote_plus(self.analyze(&rep.sub));
                self.simplify(&mut info, false);
                info
            }
        }
    }

    /// Left-fold a concatenation or alternation over its sub-expressions.
    fn fold(&self, subs: &[Hir], how: Combine) -> RegexpInfo {
        match subs {
            [] => how.zero(),
            [one] => self.analyze(one),
            [first, rest @ ..] => {
                let mut info = self.analyze(first);
                for h in rest {
                    let r = self.analyze(h);
                    info = match how {
                        Combine::Concat => self.concat(info, r),
                        Combine::Alternate => self.alternate(info, r),
                    };
                }
                info
            }
        }
    }
}

/// Which combining rule a fold applies.
#[derive(Clone, Copy)]
pub enum Combine {
    /// `xy`: concatenation.
    Concat,
    /// `x|y`: alternation.
    Alternate,
}

impl Combine {
    /// The identity element for an empty fold.
    fn zero(self) -> RegexpInfo {
        match self {
            Self::Concat => RegexpInfo::empty_string(),
            Self::Alternate => RegexpInfo::no_match(),
        }
    }
}

/// `x+`: at least one `x`, so prefixes and suffixes survive but the whole is
/// no longer an exact set.
fn demote_plus(mut info: RegexpInfo) -> RegexpInfo {
    if let Some(exact) = info.exact.take() {
        info.prefix = exact.clone();
        info.suffix = exact;
    }
    info
}

/// Describe a character class: empty matches nothing, a wide class is
/// over-approximated as any character, otherwise enumerate its members.
fn class(cls: &Class) -> RegexpInfo {
    match class_set(cls) {
        ClassSet::Empty => RegexpInfo::no_match(),
        ClassSet::Wide => RegexpInfo::any_char(),
        ClassSet::Exact(set) => RegexpInfo {
            exact: Some(set),
            ..RegexpInfo::blank()
        },
    }
}

/// The outcome of enumerating a character class.
enum ClassSet {
    Empty,
    Wide,
    Exact(StringSet),
}

fn class_set(cls: &Class) -> ClassSet {
    match cls {
        Class::Unicode(cu) => {
            let ranges: Vec<(u32, u32)> = cu
                .ranges()
                .iter()
                .map(|r| (r.start() as u32, r.end() as u32))
                .collect();
            enumerate(&ranges, encode_char)
        }
        Class::Bytes(cb) => {
            let ranges: Vec<(u32, u32)> = cb
                .ranges()
                .iter()
                .map(|r| (u32::from(r.start()), u32::from(r.end())))
                .collect();
            enumerate(&ranges, encode_byte)
        }
    }
}

fn enumerate(ranges: &[(u32, u32)], encode: fn(u32) -> Option<Vec<u8>>) -> ClassSet {
    let count: u64 = ranges
        .iter()
        .map(|&(lo, hi)| u64::from(hi - lo) + 1)
        .sum();
    if count == 0 {
        return ClassSet::Empty;
    }
    if count > MAX_CLASS {
        return ClassSet::Wide;
    }
    let mut set = StringSet::new();
    for &(lo, hi) in ranges {
        for c in lo..=hi {
            if let Some(bytes) = encode(c) {
                set.push(bytes);
            }
        }
    }
    if set.is_empty() {
        return ClassSet::Wide;
    }
    set.clean(Order::Prefix);
    ClassSet::Exact(set)
}

fn encode_char(c: u32) -> Option<Vec<u8>> {
    let mut buf = [0u8; 4];
    char::from_u32(c).map(|ch| ch.encode_utf8(&mut buf).as_bytes().to_vec())
}

fn encode_byte(c: u32) -> Option<Vec<u8>> {
    u8::try_from(c).ok().map(|b| vec![b])
}
