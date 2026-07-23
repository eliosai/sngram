# Architecture

sngram is a sparse n-gram engine for regex prefiltering. It has two jobs:
scan text into selective grams, and fold regex patterns into safe query
plans over those grams. An index built from scans answers a plan with
candidate documents; the caller verifies candidates with the real regex
engine. The index may return false positives. It must never miss a match.

## Crates

| crate | role |
|---|---|
| `crates/types` | shared data shapes: `WeightTable`, `GramKey`, `ScanEvent`, `QueryPlan`, `GramNeedle`, `ScanNeed` |
| `crates/lib` | the public API: `sngram::scan`, `sngram::query`, the embedded production table behind `weights`, and training counters behind `learn` |
| `crates/python` | the standalone `sngram` Python package: scan, query, weight tables, and GIL-free training counters |
| `crates/eg` | the application: a ripgrep fork that prefilters through the index, plus the `eg-indexd` daemon |

The training pipeline lives outside the workspace in `train/`, a uv
project that depends on the Python package. It draws the published
corpus manifest from the Hugging Face Hub and mints the production
weight table.

## How a gram is chosen

The scanner slides over the text and keeps a gram when the bigram weights
at its borders are strictly greater than every interior bigram weight (the
valley rule), plus all trigrams. Weights come from a trained 256×256
byte-pair table, so gram borders land on rare byte pairs and the kept
grams are selective. The algorithm was differentially verified against
danlark1/sparse_ngrams, the reference implementation of the Google/GitHub
rule: 1905 cases with exact gram-set equality.

Two properties make the scheme sound for search. A gram's emission depends
only on the bytes inside its span, so every gram the scanner emits for a
string alone is also emitted for any document containing that string. And
the cover of a string (its minimal covering chain) is a subset of its
scan, so a plan built from covers only demands grams the index has.

## How a query becomes a plan

`sngram::query` parses the regex into HIR, folds it bottom-up into
prefix/suffix/exact string sets (Google codesearch's structure with
sparse-native covers), and flushes those sets into a boolean tree of gram
requirements: `AllOf`/`AnyOf` over `GramNeedle`s. The root also carries
`ScanNeed`s, cheap per-document facts a match provably implies: minimum
byte length, byte counts, required byte sets, line-edge bytes, document
edges, and the longest-line floor for patterns that cannot match a
newline. See [query-planning.md](query-planning.md).

## How eg answers a query

The foreground `eg` process plans the pattern, opens the daemon-proofed
index for the search root, executes the plan against the postings, and
verifies the candidate files through the copied ripgrep search path. Plan
execution intersects posting lists and their per-document masks: five
hashed line-bucket bits prune grams that never share a line region, and
word-edge bits prune `-w` queries whose grams only occur mid-word. See
[index-format.md](index-format.md) for the bytes and
[daemon.md](daemon.md) for who builds and owns the index.

Queries the index cannot narrow fall back to a full scan: patterns that
plan to nothing, and plans whose candidate estimate exceeds 30% of the
corpus, where verification would cost more than scanning.

## Provenance

The CLI surface and search path are copied from ripgrep at a pinned
commit ([ripgrep-upstream.md](ripgrep-upstream.md)). The query algebra
follows Google codesearch, rebuilt for sparse grams instead of trigrams.

## Measured endline

On a 1.59GB Linux kernel checkout: the index is 1.42GB (0.90x the
corpus), the 296-query suite runs 2.4x faster than scanning with 27.8%
aggregate false positives and zero false negatives. The record of how it
got there, including every rejected design, is
[fp-optimization-plan.md](fp-optimization-plan.md).
