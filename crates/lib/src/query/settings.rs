//! Query parser settings.

use crate::scan::settings::ScanSettings;

/// Internal parser constants matching the verifier defaults this crate plans
/// against.
#[derive(Debug, Clone, Copy)]
pub struct QuerySettings;

impl QuerySettings {
    /// Maximum accepted regex pattern length in bytes.
    pub const MAX_PATTERN_LEN: usize = 4096;

    /// Shortest gram that the scanner can emit.
    pub const MIN_GRAM_LEN: usize = ScanSettings::MIN_GRAM_LEN;

    /// Query plans may include line-boundary sentinels when the scanner emits
    /// matching document sentinels.
    pub const LINE_SENTINELS: bool = ScanSettings::DOCUMENT_SENTINELS;

    /// Query plans may include folded keys when the scanner emits folded
    /// supplement grams.
    pub const CASE_FOLDED_SUPPLEMENTS: bool = ScanSettings::CASE_FOLDED_SUPPLEMENTS;

    /// Nest limit matching `grep-regex`'s translator, so any pattern the
    /// verifier accepts also parses here.
    pub const VERIFIER_NEST_LIMIT: u32 = 250;

    /// Octal escapes are disabled to match the verifier.
    pub const OCTAL: bool = false;

    /// Byte regexes are allowed. Unicode stays enabled by default, but inline
    /// `(?-u:...)` can opt out for callers that need byte-mode syntax.
    pub const UTF8: bool = false;

    /// Multiline anchors are always enabled. This is sound for non-multiline
    /// verifiers because it only makes anchor-based pruning less aggressive.
    pub const MULTI_LINE: bool = true;

    /// Case-insensitive matching is opt-in through inline regex flags.
    pub const CASE_INSENSITIVE: bool = false;

    /// Dot does not match newlines unless the pattern opts in with `(?s:...)`.
    pub const DOT_MATCHES_NEW_LINE: bool = false;

    /// CRLF mode is off unless the pattern opts in with `(?R:...)`.
    pub const CRLF: bool = false;

    /// Unicode mode is on unless the pattern opts out with `(?-u:...)`.
    pub const UNICODE: bool = true;
}
