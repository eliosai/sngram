# Query planning

`sngram::query(&table, pattern)` folds a regex into a `QueryPlan`: a
boolean tree over gram requirements and per-document facts. The contract
is one-directional. Every document the regex matches satisfies the plan;
the plan may admit documents the regex rejects, and the verifier removes
those.

## Covers

The planner constrains a literal by the grams the scanner is guaranteed
to emit for it. Gram emission depends only on the bytes inside a gram's
span, so scanning the literal alone yields grams present in any document
containing it. The planner uses the maximal guaranteed set when branch
counts are small and falls back to minimal covering chains, then to the
strongest single gram per branch, as budgets tighten.

## Folding the regex

The fold is Google codesearch's algebra rebuilt for sparse grams. Each
HIR node summarizes into exact/prefix/suffix string sets plus a match
query; concat crosses sets at the seam, alternation unions them, and
`simplify` bounds growth by flushing covers into the match query before
truncating the sets. Two sparse-native differences from codesearch:
literals cover to full guaranteed gram sets instead of trigrams, and
boundary windows stay wide after a flush instead of degrading to
two-byte stubs.

Concat seams matter most. The strings straddling a concat boundary are
suffix(left) × prefix(right); their covers catch patterns whose halves
are individually common but rare together. Oversized seam sides shrink
to their longest distinct edge truncation instead of dropping the seam.

Case-insensitive queries plan once in a folded gram space (scan emits an
ASCII-folded twin stream under salted keys) instead of exploding case
variants into OR branches.

## Scan needs

The root carries `ScanNeed`s, facts a match provably implies, answered
from 240-byte document summaries without touching content:

- `MinByteLen`, `MinByteCounts`: length and saturating per-byte counts
  the match requires.
- `ContainsAnyByte`: UTF-8 lead-byte sets from required character
  classes, capped at 4 sets of 128 bytes.
- `MinLongestLineLen`: emitted when the HIR proves the pattern cannot
  match a newline, so the match fits on one line.
- `LineStartsWithAnyByte` / `LineEndsWithAnyByte` from `^`/`$` anchors,
  skipped when the edge set could contain a newline.
- `StartsWith` / `EndsWith` from `\A`/`\z` literal edges.

Anchors also work through virtual sentinels: scan brackets every document
with `\n`, so `^foo` demands the sentinel-bridging gram `\nfoo`.

## Word boundaries

`\b literal \b` patterns lower edge-aligned cover grams to
`GramNeedle::AtWordEdge`. A gram that starts the literal demands a
posting whose WORD_START bit is set; one that ends it demands WORD_END; a
gram spanning the whole literal demands WORD_BOTH, a single occurrence
carrying both edges. eg rewrites `-w` into `\b(?:pattern)\b` and `-x`
into `^(?:pattern)$` before planning, so the flags reach this path.

## Tuning and execution

At query time eg clones the plan and tunes it against the live index:
needle keys sort by document frequency, `AllOf` grams drop when their
estimated frequency exceeds the selectivity ceiling (keeping at least
one), and pure-gram `AnyOf` children whose summed frequency cannot prune
drop when a stronger sibling remains.

The executor evaluates the tree over posting lists. `AllOf` intersects,
ANDing masks and discarding documents with no shared line bucket;
skewed intersections gallop through the longer list. `AnyOf` unions,
ORing masks. Needs filter survivors through the summaries. Plans whose
estimate exceeds 30% of the corpus are refused and the query falls back
to a scan, since verifying that many candidates costs more than
scanning; the estimator gets one exact-lookup refinement pass when its
additive guess lands within 2x of the ceiling.

## Soundness gates

`crates/lib/tests/soundness.rs` sweeps plans against a multi-line regex
oracle; `differential.rs` pins the scanner to the reference
implementation; the eg suite fails any run where indexed hits diverge
from scan hits. Constraints are justified from first principles, never
fitted to the benchmark queries.
