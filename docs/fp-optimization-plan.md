# FP optimization plan

Goal: the smallest index that minimizes false positives across the general query space.
Optimize the sngram library, the eg index shape, and query execution together.

## Baseline (2026-07-07, ~/ripos/linux, 242-query embedded suite)

- Aggregate FP: 33.51% (640,009 candidates, 425,567 matches, 214,442 false positives)
- Indexed vs scan wall: 1.55x, vs rg: 1.49x
- Index: ~7.0GB mmap for ~1.5GB corpus — table.bin 3.97GB (198M gram records, 20B each), postings.bin 2.94GB (raw u32 doc ordinals), summaries 37MB
- Unsupported (fallback to scan): 34/242 queries via the 30% selectivity ceiling

Worst FP classes:

| class | example | fp_pct |
|---|---|---|
| gap queries | `static.*return -E` | 99.67 |
| wide Unicode class seams | `[A-Za-z\p{Greek}]term_var` | 100.00 |
| rare-literal gram scatter | `sched_clock` | 90.04 |
| case probes | `TaskStruct` | 100.00 |

## Algorithm correctness (settled)

The scanner was differentially verified against danlark1/sparse_ngrams, the reference
implementation of the Google/GitHub sparse-gram rule: 1905 cases, exact gram-set and
cover equality under shared weight functions, tie-breaking equivalent, cover ⊆ scan
held everywhere, stack eviction lossless. Divergences from the reference are deliberate
and sound: MAX_GRAM_LEN gate matched by cover force-split, sentinel bracketing,
case-folded supplements. The core algorithm is not the problem; index shape and plan
execution are.

## Structural findings

1. Postings are doc-granular. Gram AND is set membership, so co-occurrence, order,
   and adjacency are invisible. This is the entire 90–99% FP tail.
2. 10 of 12 ScanNeed predicates are dead. Scan computes them, summaries persist them
   (400B/doc), the executor evaluates them, the planner never emits them:
   StartsWith/EndsWith, LineStartsWithAnyByte/LineEndsWithAnyByte, HasFlags,
   MinLineCount, MinEmptyLineCount, MinLongestLineLen, ContainsAllBytes,
   ContainsAnyByte. Needs also attach only at the plan root, never per branch.
3. MAX_GRAM_LEN=100 makes long grams nearly all unique: each costs a 20B table record
   for a ~1-doc posting. The table is larger than the postings.
4. Nothing on disk is compressed: postings are raw u32, table records fixed 20B.
5. Default matching is line-oriented, so every covering gram of a match lies on the
   match line (sentinel-bridging grams touch the adjacent line). Line-granular
   co-occurrence is a sound constraint everywhere except -U.

## Decisions (grill, 2026-07-07)

- Index shape: line-block granularity postings. Full Zoekt positions rejected (size);
  doc-only rejected (structural FP floor).
- Gram cap: bench sweep {16, 24, 32, 64}, expect the knee near 32.
- Size budget: index ≤ 50% of corpus text bytes (linux: ≤ ~0.75GB, from 7GB).
- Weight tables: mint-time boundary-tuning sweeps are in scope and must complete
  before the final training run; no retraining during this round.
- Format changes: version bump + destructive daemon rebuild, no migration readers.
- Perf guardrail: end-to-end suite wall stays ≥ baseline speedups; lookup may pay CPU
  when verify savings cover it; understand every tradeoff, optimize toward minimal FP.
- Corpora: linux is the optimization corpus; one structurally different guard corpus
  (mixed-language checkout) runs at phase boundaries and must hold or improve.
- Process: TDD vertical slices per behavior; each phase lands as its own conventional
  commit with before/after suite numbers.

## Phases

### Phase 0 — measurement hardening

- Enrich `crates/eg/src/index/data/fp-queries.tsv`: more gap shapes, anchors,
  word-boundary, `-i`/`-w`/`-U`/`--crlf` modes, non-C-shaped queries (prose, paths,
  JSON keys), zero-match probes per class.
- Suite output gains per-class FP aggregation (label prefix) and index
  bytes-per-corpus-byte.
- Make the FN guard explicit: any `hit != scan_hit` row fails the suite run.
- Pick and wire the guard corpus (env or flag on the suite).

### Phase 1 — wire the dead ScanNeeds (free precision)

Planner emits every need it can prove; index format unchanged.

- `LineStartsWithAnyByte`/`LineEndsWithAnyByte` from `^`/`$` boundary byte sets.
- `StartsWith`/`EndsWith` from `\A`/`\z` exact prefixes/suffixes.
- `HasFlags` from required byte classes (digit, upper, non-ascii, crlf).
- `MinLineCount` from required `\n` literals.
- Per-branch needs on AnyOf/AllOf children, not root-only.
- Forced candidates get filtered by the same needs (already plumbed, verify).
- Watch: AnyOf-need union paths walk all summaries, O(doc_count); keep needs on
  candidate-bounded paths or bench the walk.

### Phase 2 — shrink the index

- Gram-cap sweep {16, 24, 32, 64}: scan gates, cover force-splits at the same cap
  (soundness argument identical to today's 100). Pick by FP/size/latency knee on
  linux, sanity-check on guard corpus.
- Postings: delta + varint (or stream-vbyte) doc lists.
- Table: shrink records (hash-bucketed layout, narrower offsets, derive len).
- Target after this phase: ≤ 50% of corpus, before line-blocks are added back in.

### Phase 3 — line-block postings (postings-v6)

- Each posting carries coarse intra-doc location; candidate encodings to bench:
  fixed 64-bucket bitmap per (gram, doc) vs (doc, block-id) pairs.
- A gram spanning a line boundary sets both blocks (sentinel/anchor grams stay sound).
- Executor: AllOf intersects block sets within the plan's line scope; empty block
  intersection rejects the doc. AnyOf unions. `-U` and any multiline plan drop block
  constraints to doc granularity.
- eg scan path attributes each gram span to blocks via the newline offsets it already
  tracks for summaries.
- Expected: gap queries 99% → ~0, scatter 90% → <10.

### Phase 4 — mint-time tuning sweep (gates the final training run)

- Re-mint from saved trainer counts across a boundary_discount/floor grid.
- Rebuild + full suite per variant on linux; guard corpus on finalists.
- Deliverable: the tuning setting the final training run will bake in.

### Phase 5 — residual classes

- Wide-class seams: boundary-byte-set representation so the seam contributes a
  constraint instead of nothing.
- Case-probe FPs: audit folded-supplement admission on case-sensitive plans.
- Revisit `tune()` constants (MAX_ALL_OF_GRAMS=3) and the 30% selectivity ceiling
  once block precision changes the cost model.

## Targets

- Aggregate suite FP: 33.5% → single digits; gap and scatter classes → <10% each.
- Index size: ≤ 50% of corpus text bytes.
- Zero false negatives, always (suite-enforced).
- End-to-end suite speedups ≥ baseline (1.55x scan / 1.49x rg).
