# sngram

sngram is a sparse n-gram engine for regex prefiltering: a weight table scores every byte pair,
`scan` turns a document into the grams an inverted index stores, and `query` folds a regex into a
boolean plan over those grams that never drops a match. `crates/lib` is the `sngram` crate and
`crates/eg` is `elgrep`, the indexed ripgrep built on it; both publish to crates.io. `crates/python`
is the `sngram` Python package and `crates/lib/benches` the bench crate; neither publishes there.

## Layout

- `crates/lib/src` is the crate root and four private chapters: `table` holds the `SPNG` weight
  table and its checksum, `scan` the monotonic-stack scanner, the summary facts and the binary rule,
  `query` the regex analysis, the covering and the plan, and `learn` the bigram counters behind the
  `learn` feature; `bytes.rs` and `hashing.rs` are the primitives `scan` and `query` share
- `crates/lib/src/weights.rs` embeds `data/weights.bin`, the trained production table, behind the
  default `weights` feature, and `crates/lib/tests` holds the integration tests, which reach only the
  public API
- `crates/eg/src` is the ripgrep fork with the index in front: `index/` plans, builds, reads and
  verifies, `index/daemon/` is `eg-indexd`, and `docs/ripgrep-upstream.md` says what was copied
- `crates/python` is the maturin project: the pyo3 crate in `src`, the `sngram` package, its tests,
  and the CodSpeed benchmarks under `benchmarks`; `train/` is the separate `sngram-train` uv project
  that mints tables from the corpus
- `docs/` explains what the code cannot: `api.md` the public surface, `architecture.md` the crates,
  `query-planning.md` the plan algebra, `index-format.md` and `daemon.md` elgrep's index,
  `training.md` and `training-data.md` the corpus, `benchmarking.md` the numbers, `releasing.md` the
  pipeline, `todo.md` the open work
- `scripts/` holds the checks `just` runs, and `.github/workflows` runs the same recipes

## Commands

Every task has a `just` recipe, and the gate runs nothing a recipe does not run. `just check` scans
docs and layout, format-checks, then type-checks and lints the workspace, and the `sngram` crate with
every feature and with none, with warnings denied. `just test` runs nextest, `just test-doc` the doc
examples, `just doc-check` the docs.rs build, `just package-check` the crates.io packages,
`just semver-check` the `sngram` API against a baseline, `just msrv` the 1.96 build, `just audit`
cargo-deny, `just bench` the library benches the way CodSpeed runs them, `just py-lint`,
`just py-test` and `just wheel` the Python side, and `just ci` all of it. `just hooks` installs the
prek hooks, so a commit runs `just check` and a push runs `just test`.

## The Index Never Misses

A plan may admit a document the regex rejects and never the reverse. `crates/lib/tests/soundness.rs`
and `precision.rs` prove that against a regex oracle, `differential.rs` proves `scan` emits every key
a reference scanner emits, and `scan_bit_identity` pins the scanner to a frozen copy of itself byte
for byte. Change a scan or plan rule with its test first, then the code that makes the test pass.

`scan` applies no binary policy. The library offers one, `is_binary`, true for a NUL byte in the
first 8 KiB, which is a prefix of the rule ripgrep's verifier quits on. elgrep needs no policy: it
indexes the bytes before the first NUL, the only bytes its verifier can report a match in.

## Comments And Prose

A comment is one line that says what the item is, without a trailing period, and never who uses it
or why it exists. `scripts/comment-scan.sh` counts the comments that still run past one line; the
count is not yet a gate, so do not add to it.

The crate doc is `crates/lib/README.md`, pulled in with `include_str!`, so every README example is
a doc test. Project prose uses active voice, concrete terms, and short paragraphs. Read `stop-slop`
and `josh-voice` before writing it. Do not add speculative architecture or duplicate an existing
document.

## Visibility

Never write `pub(crate)`, `pub(super)`, `pub(in path)`, or any other restricted visibility. An item
is a plain private `fn` unless another module needs it, and then it is `pub`. Keep the private form
whenever it still compiles. `pub use` lives only in `lib.rs`.

The public surface of `sngram` is the crate root: the four functions, every type they take or
return, and `DfStats`. `learn` is the one public module, for what only the trainer needs, and
`docs/api.md` lists every public item. `ScanSummary`, `ScannedGram` and `ByteRange` are open records marked
`#[non_exhaustive]`; every other public struct keeps its fields private behind accessors. The error
enums are `#[non_exhaustive]`. `PlanExpr`, `GramNeedle` and `ScanNeed` are exhaustive on purpose:
an executor must handle every variant, so a new variant is a breaking change and gets a major bump.

## Code Quality

- keep functions and methods at 25 lines or less and files under 400 lines; split by behavior first;
  `crates/eg` carries 22 files over 400 lines from ripgrep, and a change does not add to them
- never pair `x.rs` with an `x/` directory, and never use `#[path]`; a module with children is `x/mod.rs`
- production code returns errors; `unwrap`, `expect` and `panic` live only in tests
- the scanner allocates nothing per byte, and a gram callback receives a `Copy` value
- everything public is typed: a domain type over a loose integer, string, bool or byte buffer
- read `rust-best-practices`, `coding-guidelines` and `stop-slop` before every Rust change, and
  `codebase-design` before changing a module boundary or a public signature

## Tests

- integration tests live in `crates/lib/tests` and `crates/eg/tests` and exercise the public API
  alone; a unit test lives in `#[cfg(test)] mod tests` inside the file that owns the logic it exercises
- run tests with `cargo nextest`, never `cargo test`; doc examples run through `just test-doc`
- `just test` runs elgrep last and alone, because its daemon vouches within 30 ms and loses under load
- every test proves one exact behavior
- the Python tests run with `uv run pytest` in `crates/python` and `train`
- read `tdd` and `rust-testing` before changing behavior or tests

## Benchmarks

`crates/lib/benches` runs on Divan through the CodSpeed compatibility layer and `just bench` runs it
the way the `bench` workflow does. `crates/python/benchmarks` runs under `pytest-codspeed` through
`just py-bench`. `eg --bench PATTERN PATH` reports one indexed query against `--no-index` and `rg`,
and bare `eg --bench` runs the embedded 296-query suite; `docs/benchmarking.md` has the numbers.

## Releases

Every merge to `main` may release. The release workflow reads the commits since the last tag: a
breaking `sngram` API change reported by cargo-semver-checks or a `!` subject bumps the major, which
before 1.0 moves the minor, a `feat` the minor, a `fix`, `perf` or `refactor` the patch, and anything
else ships nothing. It publishes `sngram` and then `elgrep`. A breaking pull request carries the
`semver-major` label or the semver gate fails it. `docs/releasing.md` has the rest.

## Skills

Read the matching skill in `.agents/skills` before touching its domain. `.claude/skills` points to
the same directory.

- `stop-slop` and `josh-voice` for prose, `tdd` and `rust-testing` for tests
- `rust-best-practices`, `coding-guidelines` and `codebase-design` for Rust
- `code-review` and `thermo-nuclear-code-quality-review` for every review
- `codspeed-setup-harness` and `codspeed-optimize` for benches
- `gh-stack` for pull requests, `grilling` and `grill-with-docs` when asked to stress-test a
  decision, `prek` for hooks

Do not edit a copied skill as part of product work. Update skills in their own change.

## Commits

Use conventional commits. Write the subject line and stop. Keep the subject under 60 characters,
in the imperative, with no trailing period. Never add a trailer. No `Co-Authored-By`, no
`Generated-with`, no attribution of any kind. One commit does one thing, and it compiles and passes
its tests on its own.

A pull request adds at most 1000 lines, or 3000 with the `mechanical` label for verbatim moves,
renames and deletes. Only a reviewer adds `size-exempt`.

## Enforcement

`just check` runs `scripts/doc-scan.sh`, which fails on any Rust example the doc tests skip, and
`scripts/layout-scan.sh`, which fails on scoped visibility or a module file paired with a directory.
Fix a violation by deleting lines or renaming, never by rewording around the rule.
