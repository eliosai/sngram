//! Look-around satisfiability against adjacent byte context.

use regex_syntax::hir::{Hir, HirKind, Look};
use regex_syntax::try_is_word_character;

use sngram_types::Gram;

use super::super::algebra::Query;
use super::super::info::RegexpInfo;
use super::super::strings::StringSet;
use super::{Analyzer, is_word_byte};

impl Analyzer<'_> {
    /// A leading start anchor becomes its terminator bytes when the index
    /// carries line sentinels: every `^foo` occurrence in the scanned stream
    /// is preceded by a real or virtual terminator, so the plan may demand
    /// the bridging grams of `\nfoo` — the anchored-literal FP killer.
    /// Anything else stays a pending look for junction pruning
    pub fn note_look(&self, pending: &mut Vec<Look>, accs: &mut [Option<RegexpInfo>], look: Look) {
        if self.line_sentinels()
            && accs.len() == 1
            && accs.first().is_some_and(Option::is_none)
            && let Some(bytes) = start_terminators(look)
        {
            if let Some(acc) = accs.first_mut() {
                *acc = Some(terminator_info(bytes));
            }
            return;
        }
        pending.push(look);
    }

    /// Split off a trailing end anchor as its terminator info under sentinels
    pub fn split_trailing_anchor<'h>(&self, subs: &'h [Hir]) -> (&'h [Hir], Option<RegexpInfo>) {
        if !self.line_sentinels() {
            return (subs, None);
        }
        let Some((last, head)) = subs.split_last() else {
            return (subs, None);
        };
        let HirKind::Look(look) = last.kind() else {
            return (subs, None);
        };
        end_terminators(*look).map_or((subs, None), |bytes| (head, Some(terminator_info(bytes))))
    }
}

/// Whether any assertion pending between `left` and `right` is provably
/// unsatisfiable; the pending list is consumed either way. Along the way
/// each assertion FILTERS the adjacent sets: a member whose boundary byte
/// fails the assertion against every byte of the other side cannot occur in
/// a match at this junction, so it drops out — and a set filtered to
/// nothing proves the whole concatenation empty. `None` on a side means the
/// pattern edge, where any byte may precede or follow.
pub fn looks_blocked(
    pending: &mut Vec<Look>,
    mut left: Option<&mut RegexpInfo>,
    mut right: Option<&mut RegexpInfo>,
) -> bool {
    for look in pending.drain(..) {
        let left_chars = left.as_deref().and_then(last_boundaries);
        let right_chars = right.as_deref().and_then(first_boundaries);
        let filtered_empty = junction_filters_empty(
            look,
            left.as_deref_mut(),
            right.as_deref_mut(),
            left_chars.as_deref(),
            right_chars.as_deref(),
        );
        if filtered_empty || !cross_possible(look, left_chars.as_deref(), right_chars.as_deref()) {
            return true;
        }
    }
    false
}

/// Filter both junction sides against `look`; true when a known set empties
fn junction_filters_empty(
    look: Look,
    left: Option<&mut RegexpInfo>,
    right: Option<&mut RegexpInfo>,
    left_chars: Option<&[Boundary]>,
    right_chars: Option<&[Boundary]>,
) -> bool {
    if let (Some(info), Some(lb)) = (right, left_chars) {
        let keep = |b: Option<Boundary>| lb.iter().any(|&l| look_possible(look, Some(l), b));
        if filter_first_members(info, keep) {
            return true;
        }
    }
    if let (Some(info), Some(rb)) = (left, right_chars) {
        let keep = |b: Option<Boundary>| rb.iter().any(|&r| look_possible(look, b, Some(r)));
        if filter_last_members(info, keep) {
            return true;
        }
    }
    false
}

/// The terminator bytes a start anchor guarantees on its left, under sentinels
const fn start_terminators(look: Look) -> Option<&'static [u8]> {
    match look {
        Look::Start | Look::StartLF => Some(b"\n"),
        Look::StartCRLF => Some(b"\n\r"),
        _ => None,
    }
}

/// The terminator bytes an end anchor guarantees on its right, under sentinels
const fn end_terminators(look: Look) -> Option<&'static [u8]> {
    match look {
        Look::End | Look::EndLF => Some(b"\n"),
        Look::EndCRLF => Some(b"\n\r"),
        _ => None,
    }
}

/// An exact one-byte-per-terminator string set standing in for an anchor
fn terminator_info(bytes: &'static [u8]) -> RegexpInfo {
    let mut set = StringSet::new();
    for &b in bytes {
        set.push(Gram::from(&[b][..]));
    }
    RegexpInfo {
        can_empty: false,
        exact: Some(set),
        prefix: StringSet::new(),
        suffix: StringSet::new(),
        plus_base: None,
        match_: Query::all(),
    }
}

/// Whether `look` can hold for at least one adjacent boundary pair; a `None`
/// side stands for unknown context (or the haystack edge).
fn cross_possible(look: Look, left: Option<&[Boundary]>, right: Option<&[Boundary]>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(lb), None) => lb.iter().any(|&l| look_possible(look, Some(l), None)),
        (None, Some(rb)) => rb.iter().any(|&r| look_possible(look, None, Some(r))),
        (Some(lb), Some(rb)) => lb
            .iter()
            .any(|&l| rb.iter().any(|&r| look_possible(look, Some(l), Some(r)))),
    }
}

/// Drop `info`'s exact/prefix members whose FIRST byte fails `keep`;
/// returns true when a known non-empty set filtered to nothing (no match
/// can pass the junction). Empty-string members have no first byte at this
/// junction and always survive.
fn filter_first_members(info: &mut RegexpInfo, keep: impl Fn(Option<Boundary>) -> bool) -> bool {
    let set = match info.exact.as_mut() {
        Some(exact) => exact,
        None => &mut info.prefix,
    };
    if set.is_empty() {
        return false;
    }
    set.retain(|m| keep(first_boundary(m.as_bytes())));
    set.is_empty()
}

/// Symmetric to [`filter_first_members`] for exact/suffix LAST bytes.
fn filter_last_members(info: &mut RegexpInfo, keep: impl Fn(Option<Boundary>) -> bool) -> bool {
    let set = match info.exact.as_mut() {
        Some(exact) => exact,
        None => &mut info.suffix,
    };
    if set.is_empty() {
        return false;
    }
    set.retain(|m| keep(last_boundary(m.as_bytes())));
    set.is_empty()
}

/// The complete set of boundary characters a match of `info` can end with,
/// or `None` when unknown. The suffix (or exact) set holds a string every
/// match ends with, so the members' last characters are exhaustive; an
/// empty-able match has no final character to speak of.
fn last_boundaries(info: &RegexpInfo) -> Option<Vec<Boundary>> {
    if info.can_empty {
        return None;
    }
    let set = info.exact.as_ref().unwrap_or(&info.suffix);
    boundaries(set.as_slice().iter().map(|s| last_boundary(s.as_bytes())))
}

/// The complete set of boundary characters a match of `info` can start with;
/// see [`last_boundaries`].
fn first_boundaries(info: &RegexpInfo) -> Option<Vec<Boundary>> {
    if info.can_empty {
        return None;
    }
    let set = info.exact.as_ref().unwrap_or(&info.prefix);
    boundaries(set.as_slice().iter().map(|s| first_boundary(s.as_bytes())))
}

fn boundaries(members: impl Iterator<Item = Option<Boundary>>) -> Option<Vec<Boundary>> {
    let mut out = Vec::new();
    for boundary in members {
        out.push(boundary?);
    }
    if out.is_empty() { None } else { Some(out) }
}

/// The adjacent byte plus any scalar-level wordness proven from a complete
/// UTF-8 character at a regex boundary.
#[derive(Clone, Copy)]
struct Boundary {
    byte: u8,
    ascii_word: bool,
    unicode_word: Option<bool>,
}

fn first_boundary(bytes: &[u8]) -> Option<Boundary> {
    let byte = *bytes.first()?;
    Some(Boundary {
        byte,
        ascii_word: byte.is_ascii() && is_word_byte(byte),
        unicode_word: first_char(bytes).and_then(unicode_word),
    })
}

fn last_boundary(bytes: &[u8]) -> Option<Boundary> {
    let byte = *bytes.last()?;
    Some(Boundary {
        byte,
        ascii_word: byte.is_ascii() && is_word_byte(byte),
        unicode_word: last_char(bytes).and_then(unicode_word),
    })
}

fn first_char(bytes: &[u8]) -> Option<char> {
    let byte = *bytes.first()?;
    let len = utf8_len(byte)?;
    let slice = bytes.get(..len)?;
    let text = core::str::from_utf8(slice).ok()?;
    text.chars().next()
}

fn last_char(bytes: &[u8]) -> Option<char> {
    let mut start = bytes.len().checked_sub(1)?;
    while start > 0
        && bytes
            .get(start)
            .is_some_and(|b| b & 0b1100_0000 == 0b1000_0000)
    {
        start -= 1;
    }
    let slice = bytes.get(start..)?;
    let text = core::str::from_utf8(slice).ok()?;
    let mut chars = text.chars();
    let ch = chars.next()?;
    if chars.next().is_none() {
        Some(ch)
    } else {
        None
    }
}

const fn utf8_len(byte: u8) -> Option<usize> {
    match byte {
        0x00..=0x7F => Some(1),
        0xC2..=0xDF => Some(2),
        0xE0..=0xEF => Some(3),
        0xF0..=0xF4 => Some(4),
        _ => None,
    }
}

fn unicode_word(ch: char) -> Option<bool> {
    try_is_word_character(ch).ok()
}

/// Whether `look` can hold between one concrete boundary pair. `None` means
/// the boundary is absent or unknown; unknown keeps the assertion possible.
/// Sound to over-report: a spurious `true` only costs candidates.
fn look_possible(look: Look, left: Option<Boundary>, right: Option<Boundary>) -> bool {
    match look {
        // text anchors: no byte may sit on the anchored side
        Look::Start => left.is_none(),
        Look::End => right.is_none(),
        // line anchors: the adjacent byte, if any, must be a terminator
        Look::StartLF => left.is_none_or(|b| b.byte == b'\n'),
        Look::EndLF => right.is_none_or(|b| b.byte == b'\n'),
        Look::StartCRLF => left.is_none_or(|b| b.byte == b'\n' || b.byte == b'\r'),
        Look::EndCRLF => right.is_none_or(|b| b.byte == b'\n' || b.byte == b'\r'),
        Look::WordAscii | Look::WordAsciiNegate | Look::WordUnicode | Look::WordUnicodeNegate => {
            word_look_possible(look, left, right)
        },
        _ => true,
    }
}

/// Word boundaries hold when wordness differs, negations when it agrees.
///
/// Unicode wordness is used only when a complete adjacent scalar is known;
/// invalid or truncated UTF-8 stays unknown.
const fn word_look_possible(look: Look, left: Option<Boundary>, right: Option<Boundary>) -> bool {
    let (l, r) = match look {
        Look::WordAscii | Look::WordAsciiNegate => (ascii_word_of(left), ascii_word_of(right)),
        _ => (unicode_word_of(left), unicode_word_of(right)),
    };
    let (Some(l), Some(r)) = (l, r) else {
        return true;
    };
    if matches!(look, Look::WordAscii | Look::WordUnicode) {
        l != r
    } else {
        l == r
    }
}

const fn ascii_word_of(boundary: Option<Boundary>) -> Option<bool> {
    match boundary {
        Some(b) => Some(b.ascii_word),
        None => None,
    }
}

const fn unicode_word_of(boundary: Option<Boundary>) -> Option<bool> {
    match boundary {
        Some(b) => b.unicode_word,
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Look, first_boundary, last_boundary, look_possible};

    #[test]
    fn unicode_word_boundary_uses_complete_scalars_only() {
        let word_left = last_boundary("α".as_bytes());
        let word_right = first_boundary("β".as_bytes());
        let space_right = first_boundary(b" ");
        let incomplete_right = first_boundary(&[0xCE]);

        assert!(!look_possible(Look::WordUnicode, word_left, word_right));
        assert!(look_possible(Look::WordUnicode, word_left, space_right));
        assert!(
            look_possible(Look::WordUnicode, word_left, incomplete_right),
            "truncated UTF-8 must stay possible instead of proving no match"
        );
    }
}
