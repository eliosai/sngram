//! Character-class analysis into exact or wide summaries.

use regex_syntax::hir::Class;

use sngram_types::Gram;

use super::super::info::RegexpInfo;
use super::super::strings::{Order, StringSet};
use super::boundary::{BoundaryFn, byte_boundary_bytes, utf8_boundary_bytes};

/// Character-class size past which enumeration stops and over-approximates.
const MAX_CLASS: u64 = 100;

/// Describe a character class: empty matches nothing, a wide class keeps its
/// first- and last-byte sets (over-approximating the middle as any character),
/// otherwise enumerate its members.
///
/// A wide class is not collapsed to a bare `any_char`: its `prefix`/`suffix`
/// carry every byte a match can start or end with, as one-byte members. These
/// slot into the ordinary seam and cross machinery — a wide class next to a
/// literal forms `<boundary-byte>literal` windows the plan can cover — while a
/// side whose byte set is too large falls back to `{""}` (boundary unknown),
/// leaving that edge as unconstraining as before.
pub fn class(cls: &Class) -> RegexpInfo {
    info_from_class_set(class_set(cls))
}

fn info_from_class_set(set: ClassSet) -> RegexpInfo {
    match set {
        ClassSet::Empty => RegexpInfo::no_match(),
        ClassSet::Wide { first, last } => RegexpInfo {
            prefix: first,
            suffix: last,
            ..RegexpInfo::blank()
        },
        ClassSet::Exact(set) => RegexpInfo {
            exact: Some(set),
            ..RegexpInfo::blank()
        },
    }
}

/// Split a mixed-width Unicode class into a small exact ASCII branch and a
/// residual non-ASCII branch. This keeps the ASCII branch's first/last byte
/// correlation until neighbouring literals are crossed, avoiding a merged
/// wide-class plan that can use different class bytes on each side.
pub fn split_mixed_class(cls: &Class) -> Option<(RegexpInfo, RegexpInfo)> {
    let ranges = unicode_ranges(cls)?;
    if range_count(&ranges) <= MAX_CLASS {
        return None;
    }
    let (ascii, non_ascii) = partition_ascii_ranges(&ranges);
    let ascii_count = range_count(&ascii);
    if ascii_count == 0 || ascii_count > MAX_CLASS || non_ascii.is_empty() {
        return None;
    }
    let ascii = info_from_class_set(enumerate(&ascii, encode_char, utf8_boundary_bytes));
    let non_ascii = info_from_class_set(enumerate(&non_ascii, encode_char, utf8_boundary_bytes));
    Some((ascii, non_ascii))
}

type ScalarRange = (u32, u32);
type ScalarRanges = Vec<ScalarRange>;

fn unicode_ranges(cls: &Class) -> Option<ScalarRanges> {
    let Class::Unicode(cu) = cls else {
        return None;
    };
    Some(
        cu.ranges()
            .iter()
            .map(|r| (r.start() as u32, r.end() as u32))
            .collect(),
    )
}

fn partition_ascii_ranges(ranges: &[ScalarRange]) -> (ScalarRanges, ScalarRanges) {
    let mut ascii = Vec::new();
    let mut non_ascii = Vec::new();
    for &(lo, hi) in ranges {
        if lo <= 0x7F {
            ascii.push((lo, hi.min(0x7F)));
        }
        if hi >= 0x80 {
            non_ascii.push((lo.max(0x80), hi));
        }
    }
    (ascii, non_ascii)
}

fn range_count(ranges: &[(u32, u32)]) -> u64 {
    ranges.iter().map(|&(lo, hi)| u64::from(hi - lo) + 1).sum()
}

/// The outcome of enumerating a character class.
enum ClassSet {
    Empty,
    /// Over the [`MAX_CLASS`] enumeration cap: the exact set is dropped, but
    /// the exhaustive first-/last-byte sets are kept as one-byte members
    /// (each `{""}` when its side overflowed the boundary cap).
    Wide {
        first: StringSet,
        last: StringSet,
    },
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
            enumerate(&ranges, encode_char, utf8_boundary_bytes)
        },
        Class::Bytes(cb) => {
            let ranges: Vec<(u32, u32)> = cb
                .ranges()
                .iter()
                .map(|r| (u32::from(r.start()), u32::from(r.end())))
                .collect();
            enumerate(&ranges, encode_byte, byte_boundary_bytes)
        },
    }
}

/// Enumerate a class into its exact set, or — once it exceeds [`MAX_CLASS`] —
/// its first/last boundary-byte sets via `boundary`.
fn enumerate(
    ranges: &[(u32, u32)],
    encode: fn(u32) -> Option<Gram>,
    boundary: BoundaryFn,
) -> ClassSet {
    let count = range_count(ranges);
    if count == 0 {
        return ClassSet::Empty;
    }
    if count > MAX_CLASS {
        return wide(ranges, boundary);
    }
    exact(ranges, encode).map_or_else(|| wide(ranges, boundary), ClassSet::Exact)
}

fn wide(ranges: &[(u32, u32)], boundary: BoundaryFn) -> ClassSet {
    let (first, last) = boundary(ranges);
    ClassSet::Wide { first, last }
}

fn exact(ranges: &[(u32, u32)], encode: fn(u32) -> Option<Gram>) -> Option<StringSet> {
    let mut set = StringSet::new();
    for &(lo, hi) in ranges {
        for c in lo..=hi {
            if let Some(bytes) = encode(c) {
                set.push(bytes);
            }
        }
    }
    if set.is_empty() {
        return None;
    }
    set.clean(Order::Prefix);
    Some(set)
}

fn encode_char(c: u32) -> Option<Gram> {
    let mut buf = [0u8; 4];
    char::from_u32(c).map(|ch| Gram::from(ch.encode_utf8(&mut buf).as_bytes()))
}

fn encode_byte(c: u32) -> Option<Gram> {
    u8::try_from(c).ok().map(|b| Gram::from(&[b][..]))
}

#[cfg(test)]
mod tests {
    use regex_syntax::hir::HirKind;

    use super::{Class, ClassSet, StringSet, class_set};

    /// A boundary set accepts `byte` iff it is a one-byte member, or it is the
    /// `{""}` (unknown) sentinel that accepts anything.
    fn accepts(set: &StringSet, byte: u8) -> bool {
        set.as_slice()
            .iter()
            .any(|g| g.as_bytes().is_empty() || g.as_bytes() == [byte])
    }

    /// The unicode class of `\p{name}`.
    fn script_class(name: &str) -> Class {
        let hir = regex_syntax::parse(&format!("\\p{{{name}}}")).expect("script parses");
        let HirKind::Class(class) = hir.kind() else {
            panic!("expected a unicode class");
        };
        class.clone()
    }

    #[test]
    fn greek_keeps_a_real_continuation_byte_suffix() {
        // wide and non-ASCII: both boundary sets stay real byte constraints
        let ClassSet::Wide { first, last } = class_set(&script_class("Greek")) else {
            panic!("Greek should be a wide class");
        };
        assert!(!first.as_slice().iter().any(|g| g.as_bytes().is_empty()));
        assert!(!last.as_slice().iter().any(|g| g.as_bytes().is_empty()));
        assert!(accepts(&last, 0xB1)); // last byte of U+03B1 (α)
    }
}
