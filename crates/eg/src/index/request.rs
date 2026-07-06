//! Indexed-search request validation.

use std::time::Instant;

use anyhow::bail;

use crate::flags::{HiArgs, Mode, SearchMode};

pub struct SearchRequest<'a> {
    args: &'a HiArgs,
    mode: SearchMode,
    started_at: Instant,
    matches_possible: bool,
}

impl<'a> SearchRequest<'a> {
    pub fn from_args(args: &'a HiArgs) -> anyhow::Result<Self> {
        let Mode::Search(mode) = args.mode() else {
            bail!("indexed mode only supports search");
        };
        if args.index().is_no_index() {
            return unsupported(Unsupported::Feature {
                what: "`--bench` with `--no-index`",
                why: "`--bench` measures the sparse n-gram indexed path, so disabling the index leaves no indexed work to measure",
            });
        }
        if let Some(reason) = unsupported_reason(args, mode) {
            return unsupported(reason);
        }
        if searches_stdin(args) {
            return unsupported(Unsupported::Stdin);
        }
        Ok(Self {
            args,
            mode,
            started_at: Instant::now(),
            matches_possible: args.matches_possible(),
        })
    }

    pub const fn args(&self) -> &'a HiArgs {
        self.args
    }

    pub const fn mode(&self) -> SearchMode {
        self.mode
    }

    pub const fn started_at(&self) -> Instant {
        self.started_at
    }

    pub const fn matches_possible(&self) -> bool {
        self.matches_possible
    }
}

/// Return an unindexable-query reason, or `None` when the index can serve it.
fn unsupported_reason(args: &HiArgs, _mode: SearchMode) -> Option<Unsupported> {
    if args.invert_match() {
        return Some(Unsupported::Feature {
            what: "inverted matches",
            why: "`-v/--invert-match` can make every non-matching file relevant, so sparse positive grams cannot safely narrow the search",
        });
    }
    if args.passthru() {
        return Some(Unsupported::Feature {
            what: "`--passthru`",
            why: "passthru prints non-matching lines too, so the index cannot reduce the output to matching candidate files",
        });
    }
    if args.non_default_regex_engine() {
        return Some(Unsupported::Feature {
            what: "PCRE2 or hybrid regex engines",
            why: "the sparse planner currently proves constraints for the default Rust regex semantics only",
        });
    }
    if args.explicit_encoding() {
        return Some(Unsupported::Feature {
            what: "explicit text encodings",
            why: "the index stores byte n-grams from the raw corpus and cannot yet plan over decoded alternate encodings",
        });
    }
    if args.has_preprocessor() {
        return Some(Unsupported::Feature {
            what: "preprocessors",
            why: "the index is built over stored files, not transformed preprocessor output",
        });
    }
    if args.search_zip() {
        return Some(Unsupported::Feature {
            what: "compressed archive search",
            why: "archive members are not present as stable files in the sparse n-gram index",
        });
    }
    if args.null_data() {
        return Some(Unsupported::Feature {
            what: "`--null-data`",
            why: "NUL-delimited line semantics use different boundaries than the newline sentinels stored in the sparse n-gram index",
        });
    }
    if args.index_rejects_binary_mode() {
        return Some(Unsupported::Feature {
            what: "binary search flags",
            why: "indexed eg does not search binary data; remove `--binary`/`--text` or pass `--no-index` for an explicit unindexed run",
        });
    }
    None
}

#[derive(Clone, Copy)]
pub enum Unsupported {
    Feature {
        what: &'static str,
        why: &'static str,
    },
    Stdin,
    PlannerError,
    TooBroadPattern,
    ImpossiblePattern,
    TooManyCandidates,
}

/// Report an indexed-search request that cannot be served safely.
pub fn unsupported<T>(reason: Unsupported) -> anyhow::Result<T> {
    match reason {
        Unsupported::Feature { what, why } => bail!(
            "indexed search cannot run with {what}.\n\nwhy: {why}.\nwhat works: remove the unsupported option, or pass `--no-index` when you intentionally want an exact unindexed scan."
        ),
        Unsupported::Stdin => bail!(
            "indexed search cannot read stdin.\n\nwhy: stdin is a stream, but the sparse n-gram index only covers stable files in the indexed corpus.\nwhat works: write the input to a file and search that path, or pass `--no-index` for an exact stream scan."
        ),
        Unsupported::PlannerError | Unsupported::TooBroadPattern => bail!(
            "indexed search cannot use this pattern because it is too broad for the sparse n-gram index.\n\nwhy: the pattern has no required byte n-gram that can narrow candidate files.\nwhat works: add a literal substring of at least 3 bytes, narrow wide character classes or repetitions, or pass `--no-index` for an exact unindexed scan."
        ),
        Unsupported::ImpossiblePattern => bail!(
            "indexed search cannot use this pattern because it cannot match any text under the current regex options.\n\nwhy: contradictory anchors, boundaries, or character classes made the planner prove the language empty.\nwhat works: check anchors like `$`/`^`, word boundaries like `\\b`/`\\B`, and impossible classes; use `--no-index` only if you want to double-check with the regex engine."
        ),
        Unsupported::TooManyCandidates => bail!(
            "indexed search cannot use this pattern efficiently because it selects too much of the corpus.\n\nwhy: the sparse n-gram estimate is above the indexed-search selectivity ceiling, so verifying candidates would be slower than a scan.\nwhat works: add a rarer literal, narrow numeric or wide character classes, split the search into a more selective pattern, or pass `--no-index` for an exact unindexed scan."
        ),
    }
}

/// Return true when any haystack to search is stdin.
pub fn searches_stdin(args: &HiArgs) -> bool {
    args.search_paths()
        .iter()
        .any(|path| path == std::path::Path::new("-"))
}
