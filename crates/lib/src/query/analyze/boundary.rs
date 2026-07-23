//! First- and last-byte boundary sets for wide character classes.

use sngram_types::Gram;

use super::super::strings::{Order, StringSet};

/// Distinct first- or last-bytes a wide class may keep before that side is
/// dropped to `{""}` (boundary unknown).
///
/// 128 keeps full boundary sets for mixed source-code alphabets (ASCII
/// letters plus one non-ASCII script) while still collapsing truly arbitrary
/// byte classes such as `.`/`[\x00-\xff]` to the bare-`any_char` behaviour.
/// The resulting OR remains a small fraction of the plan gram budget.
const MAX_BOUNDARY_BYTES: usize = 128;

/// Derives a wide class's (first-byte, last-byte) boundary sets from its
/// scalar or byte ranges.
pub type BoundaryFn = fn(&[(u32, u32)]) -> (StringSet, StringSet);

/// First- and last-byte sets of a byte class's members. A byte is its own
/// one-byte "encoding", so both sets are the class's bytes themselves.
pub fn byte_boundary_bytes(ranges: &[(u32, u32)]) -> (StringSet, StringSet) {
    let mut bytes = ByteSet::new();
    for &(lo, hi) in ranges {
        // byte-class endpoints are already within 0..=255
        bytes.mark_range(lo.min(0xFF) as u8, hi.min(0xFF) as u8);
    }
    let set = bytes.into_boundary();
    (set.clone(), set)
}

/// First- and last-byte sets over the UTF-8 encodings of a Unicode class's
/// scalars, derived from the scalar ranges without enumerating each scalar.
///
/// Exhaustiveness (every match starts/ends with a kept byte) holds because
/// both sets are computed as sound supersets: first bytes rise monotonically
/// with the scalar inside one UTF-8 length class, so a sub-range contributes
/// the contiguous span `[first(lo), first(hi)]`; a multi-byte last byte is a
/// continuation byte `0x80 | (cp & 0x3F)`, which cycles through all of
/// `0x80..=0xBF` once a sub-range spans 64 scalars and otherwise walks a short
/// interval. Including a byte no scalar actually produces only costs an unused
/// OR branch; it can never drop a real match.
pub fn utf8_boundary_bytes(ranges: &[(u32, u32)]) -> (StringSet, StringSet) {
    let mut first = ByteSet::new();
    let mut last = ByteSet::new();
    for &(lo, hi) in ranges {
        mark_utf8_bytes(lo, hi, &mut first, &mut last);
    }
    (first.into_boundary(), last.into_boundary())
}

/// UTF-8 length classes: `(lo, hi, byte_len)` covering all scalar values.
const UTF8_CLASSES: [(u32, u32, u8); 4] = [
    (0x0000, 0x007F, 1),
    (0x0080, 0x07FF, 2),
    (0x0800, 0xFFFF, 3),
    (0x0001_0000, 0x0010_FFFF, 4),
];

/// Mark the first and last UTF-8 bytes of every scalar in `[lo, hi]`.
fn mark_utf8_bytes(lo: u32, hi: u32, first: &mut ByteSet, last: &mut ByteSet) {
    for (clo, chi, len) in UTF8_CLASSES {
        let a = lo.max(clo);
        let b = hi.min(chi);
        if a > b {
            continue;
        }
        // first bytes rise with the scalar inside a length class
        first.mark_range(utf8_first_byte(a), utf8_first_byte(b));
        if len == 1 {
            // a one-byte scalar's last byte is the scalar itself
            last.mark_range(utf8_last_byte(a), utf8_last_byte(b));
        } else if b - a >= 63 {
            // a span of 64 scalars hits every continuation byte
            last.mark_range(0x80, 0xBF);
        } else {
            // a short multi-byte span: walk its <= 63 trailing bytes
            for cp in a..=b {
                last.mark(utf8_last_byte(cp));
            }
        }
    }
}

/// The first byte of `cp`'s UTF-8 encoding, arithmetically (no scalar
/// validity check, so it is safe across the surrogate gap).
#[allow(
    clippy::cast_possible_truncation,
    reason = "each masked value is bounded below 0x100 by construction"
)]
const fn utf8_first_byte(cp: u32) -> u8 {
    if cp < 0x80 {
        cp as u8
    } else if cp < 0x800 {
        0xC0 | (cp >> 6) as u8
    } else if cp < 0x0001_0000 {
        0xE0 | (cp >> 12) as u8
    } else {
        0xF0 | (cp >> 18) as u8
    }
}

/// The last byte of `cp`'s UTF-8 encoding: the scalar itself when ASCII, else
/// the trailing continuation byte.
#[allow(
    clippy::cast_possible_truncation,
    reason = "masked to the low 6 bits, always below 0x100"
)]
const fn utf8_last_byte(cp: u32) -> u8 {
    if cp < 0x80 {
        cp as u8
    } else {
        0x80 | (cp & 0x3F) as u8
    }
}

/// A dense set of bytes, collapsed to a boundary [`StringSet`] once complete.
struct ByteSet([bool; 256]);

impl ByteSet {
    const fn new() -> Self {
        Self([false; 256])
    }

    const fn mark(&mut self, b: u8) {
        self.0[b as usize] = true;
    }

    fn mark_range(&mut self, lo: u8, hi: u8) {
        for b in lo..=hi {
            self.mark(b);
        }
    }

    /// The marked bytes as one-byte prefix/suffix members, or `{""}` (boundary
    /// unknown) when none were marked or more than [`MAX_BOUNDARY_BYTES`] were
    /// — the latter as unconstraining as a bare `any_char`.
    fn into_boundary(self) -> StringSet {
        let count = self.0.iter().filter(|&&on| on).count();
        if count == 0 || count > MAX_BOUNDARY_BYTES {
            return StringSet::of(Gram::empty());
        }
        let mut set = StringSet::new();
        for (b, &on) in self.0.iter().enumerate() {
            if on {
                // b came from enumerate(0..256); it fits a u8
                #[allow(clippy::cast_possible_truncation, reason = "b < 256")]
                set.push(Gram::from(&[b as u8][..]));
            }
        }
        set.clean(Order::Prefix);
        set
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "tests assert by panicking; UTF-8 encodings are 1..=4 bytes"
    )]

    use regex_syntax::hir::{Class, HirKind};

    use super::{ByteSet, Gram, MAX_BOUNDARY_BYTES, StringSet, utf8_boundary_bytes};

    /// A boundary set accepts `byte` iff it is a one-byte member, or it is the
    /// `{""}` (unknown) sentinel that accepts anything.
    fn accepts(set: &StringSet, byte: u8) -> bool {
        set.as_slice()
            .iter()
            .any(|g| g.as_bytes().is_empty() || g.as_bytes() == [byte])
    }

    /// The scalar ranges of `\p{name}`.
    fn script_ranges(name: &str) -> Vec<(u32, u32)> {
        let hir = regex_syntax::parse(&format!("\\p{{{name}}}")).unwrap();
        let HirKind::Class(Class::Unicode(cu)) = hir.kind() else {
            panic!("expected a unicode class");
        };
        cu.ranges()
            .iter()
            .map(|r| (r.start() as u32, r.end() as u32))
            .collect()
    }

    /// Assert `cp`'s actual UTF-8 first and last bytes lie in the boundary sets.
    fn check_scalar(cp: u32, first: &StringSet, last: &StringSet) {
        let Some(ch) = char::from_u32(cp) else {
            return; // surrogate gap: not a scalar
        };
        let mut buf = [0u8; 4];
        let bytes = ch.encode_utf8(&mut buf).as_bytes();
        let (head, tail) = (bytes[0], bytes[bytes.len() - 1]);
        assert!(
            accepts(first, head),
            "first byte {head:#x} of U+{cp:04X} missing"
        );
        assert!(
            accepts(last, tail),
            "last byte {tail:#x} of U+{cp:04X} missing"
        );
    }

    /// Brute-force check: every scalar in `ranges` has its actual UTF-8 first
    /// and last bytes inside the derived boundary sets. This is the
    /// exhaustiveness invariant the plan's soundness rests on.
    fn assert_exhaustive(ranges: &[(u32, u32)]) {
        let (first, last) = utf8_boundary_bytes(ranges);
        for &(lo, hi) in ranges {
            for cp in lo..=hi {
                check_scalar(cp, &first, &last);
            }
        }
    }

    #[test]
    fn utf8_boundary_is_exhaustive_over_scripts() {
        for name in ["Greek", "Cyrillic", "Hebrew", "Han", "Latin"] {
            assert_exhaustive(&script_ranges(name));
        }
    }

    #[test]
    fn utf8_boundary_is_exhaustive_across_length_and_wrap_edges() {
        // ranges straddling every UTF-8 length edge, plus the full space
        assert_exhaustive(&[(0x0000, 0x0010_FFFF)]);
        assert_exhaustive(&[(0x0070, 0x0090)]); // 1->2 byte edge
        assert_exhaustive(&[(0x07F0, 0x0810)]); // 2->3 byte edge
        assert_exhaustive(&[(0xFFF0, 0x0001_0010)]); // 3->4 byte edge
        assert_exhaustive(&[(0x0C3E, 0x0C42), (0x0400, 0x04FF)]); // wrap + block
    }

    #[test]
    fn overlarge_boundary_byte_set_collapses_to_unknown_member() {
        let mut bytes = ByteSet::new();
        let last = u8::try_from(MAX_BOUNDARY_BYTES).expect("boundary cap fits in u8");
        bytes.mark_range(0, last);
        let set = bytes.into_boundary();
        assert_eq!(set, StringSet::of(Gram::empty()));
    }

    #[test]
    fn byte_set_marks_first_and_last_byte_values() {
        let mut bytes = ByteSet::new();
        bytes.mark(0);
        bytes.mark(u8::MAX);
        let set = bytes.into_boundary();

        assert_eq!(set.len(), 2);
        assert!(accepts(&set, 0));
        assert!(accepts(&set, u8::MAX));
        assert!(!accepts(&set, 1));
    }
}
