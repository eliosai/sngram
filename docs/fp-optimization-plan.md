# FP optimization plan

Goal: the smallest index that minimizes false positives across the general query space.
Optimize the sngram library, the eg index shape, and query execution together.

## Baseline (2026-07-07, ~/ripos/linux, 242-query embedded suite)

- Aggregate FP: 33.51% (640,009 candidates, 425,567 matches, 214,442 false positives)
- Indexed vs scan wall: 1.55x, vs rg: 1.49x
- Index: ~7.0GB mmap for ~1.5GB corpus: table.bin 3.97GB (198M gram records, 20B each), postings.bin 2.94GB (raw u32 doc ordinals), summaries 37MB
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
   and adjacency are invisible. This is the entire 90 to 99% FP tail.
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

### Phase 0: measurement hardening

- Enrich `crates/eg/src/index/data/fp-queries.tsv`: more gap shapes, anchors,
  word-boundary, `-i`/`-w`/`-U`/`--crlf` modes, non-C-shaped queries (prose, paths,
  JSON keys), zero-match probes per class.
- Suite output gains per-class FP aggregation (label prefix) and index
  bytes-per-corpus-byte.
- Make the FN guard explicit: any `hit != scan_hit` row fails the suite run.
- Guard corpus: ~/ripos/gitoxide (Rust, structurally unlike the kernel), via
  `just guard`; `just suite <dir>` runs any corpus.
- Simple human queries are first-class targets: plain words and phrases
  ("hello world", TODO, single -w words) get their own suite classes.

### Phase 1: wire the dead ScanNeeds (free precision)

Planner emits every need it can prove; index format unchanged.

- `ContainsAnyByte` from required classes: UTF-8 lead-byte set per class, unioned
  across alternation branches, capped at 4 sets of ≤128 bytes. Subsumes `HasFlags`
  (a required digit class ≡ the digit byte-presence set), so `HasFlags` stays unused.
- `LineStartsWithAnyByte`/`LineEndsWithAnyByte` from fully `^`/`$`-anchored patterns;
  skipped when the edge byte set could contain `\n` (empty lines record no edge byte).
- `StartsWith`/`EndsWith` from `\A`/`\z` literal edges (≤16 bytes).
- `MinLineCount` dropped: `MinByteCounts` already demands the required `\n` count,
  and the safe bound (`k` newlines ⇒ ≥`k` lines) adds nothing beyond it.
- Anchored soundness pinned by a dedicated multi-line-oracle sweep in soundness.rs
  (the verifier is line-oriented; the old oracle compiled without multi_line).
- Deferred to a later slice: per-branch needs on AnyOf/AllOf children.
- Forced candidates get filtered by the same needs (already plumbed, verify).
- Watch: AnyOf-need union paths walk all summaries, O(doc_count); keep needs on
  candidate-bounded paths or bench the walk.

### Phase 2: shrink the index (done 2026-07-07)

- Delta-varint postings (postings-v6): linux postings 2.94GB → 0.80GB.
- Gram-cap sweep {100, 64, 32, 24, 16} with full suite per point: cap 16 won every
  axis (FP 38.04→37.44, size, speed), matching the reference default. No knee above
  16; probing below 16 is open.
- 12-byte table records: truncated hash32 (collisions merge lists, sound superset,
  +31 candidates in 848k), u40 offset, advisory u24 count; run pairs 8 bytes.
- Result: linux 7.0GB → 1.68GB (ratio 1.06), guard 5.12 → 1.47, speedup 1.55x →
  1.97x, FN=0 throughout.
- The ≤0.50 ratio target needs a deeper table restructure (two-level hash directory,
  dedicated df=1 packing; table is 0.84GB for 70.5M grams). Schedule it with the
  Phase 3 format bump so corpora rebuild once.

### Phase 3: line-block postings (postings-v7, done 2026-07-07)

- Each posting carries an 8-bucket scaled line mask (+1 byte/posting); executor
  intersects masks in AllOf, unions in AnyOf, `-U` falls back to doc granularity;
  newline-spanning grams set both blocks.
- tune() keeps all grams under the stop threshold (cap 32, was 3); df-only thinning
  starved cross-literal intersections of one literal's grams entirely.
- Linux: aggregate FP 37.44 → 32.62, speedup 2.23x, FN=0, ratio 1.06 → 1.42.
  Broad wins: ci −16pp, crlf −22pp, opt −16pp, lazy −14pp, pf −13pp, field −12pp.
- Honest miss: gap stayed ~90%: spin_lock-class grams occur 20+ times per large
  file and saturate any fixed-block mask. Same force limits wide/simple residuals.
- Follow-on design (phase 3.5, after 4/5): per-line positions for selective grams
  only (df·occurrences below a budget), Zoekt's rarest-pair distance check without
  Zoekt's full positional cost. Remaining FP mass: gap 110k, simple 52k, wide 23k,
  seam 14.5k of 257k total.

### Phase 4: mint-time tuning sweep (done 2026-07-07)

- Minted discount {1, 4, 16, 64} (floor 1) from the 100GB checkpoint counts;
  rebuild + full suite per variant, same counts so only tuning varies.
- Result: untuned wins. Aggregate FP 32.50 / 33.47 / 33.85 / 34.45; tuning
  pushed 3 more queries over the selectivity ceiling and cost speed. Per class
  it trades (ci/opt/plus improve, rep/seam worsen) with a net loss.
- Why: discounting boundary bigrams aligns gram borders with identifier seams,
  producing vocabulary-shaped grams with higher df; seam-straddling grams are
  rarer and more selective. The frequency weights already encode selectivity.
- **Deliverable: the final training run mints with Tuning::OFF**, matching
  current production behavior.
- Bonus: the 100GB untuned table nearly matches the 12tb tier on this suite
  (32.50 vs 31.82); corpus scale has flat returns for FP here.

### Phase 5b: word-edge posting bits (postings-v8, done 2026-07-07)

- The simple-class FP mass was -w word queries: the index found substrings
  ("domain" for `main -w`) and neither -w nor -x ever reached the planner.
- Posting byte repacked: 6 block bits + 2 word-edge bits (occurrence borders a
  non-word byte before/after), zero size cost. `\b literal \b` patterns lower
  edge-aligned covers to GramNeedle::AtWordEdge; eg wraps -w/-x into the
  indexed pattern.
- Linux: aggregate 31.82 → 28.92, boundary 48.9 → 1.7, simple 44.8 → 21.5,
  FN=0. Guard: 49.8 → 37.1. Residual: per-occurrence pairing (START from one
  occurrence, END from another); falls to phase 3.5 positions if pursued.

### Phase 6: postings-v9, the final format (2026-07-07)

One format bump carrying every size and precision decision from the
adversarial design round. Measured findings that shaped it:

- The fixed u40 offset column made table.bin the single largest waste:
  delta-coding hash gaps and letting offsets accumulate from per-block
  directory entries collapses 12B records to ~3B.
- Max gram df on linux is 90,031, so a u16 count would truncate; counts are
  exact uvarints now (the old u24 saturation is gone).
- Varint posting gaps measured within 14% of the Elias-Fano floor; kept.
  Roaring/PEF/SIMD-BP128 all lose on the 68% df=1 gram majority.
- df=1 lists inline into their table record (ord + mask), removing one
  postings touch and the size varint.
- Masks split into a per-list column after the gaps.
- Scaled line blocks scaled with file size, so big files degraded to
  doc-granularity. Buckets are now hash(line) % 5; collision probability
  is file-size independent. The sixth bit becomes WORD_BOTH: some single
  occurrence carries both word edges, demanded by whole-literal `-w` plans
  (split START/END from different occurrences no longer slip through).
- Summaries drop dead fields (ord, line_count, empty_line_count,
  gram_count, flags) and pack byte counts as 4-bit saturating nibbles
  (15 = unbounded, over-inclusive and sound): 400B → 240B per doc.
- MinLongestLineLen is emitted for patterns that provably cannot match a
  newline; dead ScanNeed variants removed from types.
- Binary manifests skip display_path when it equals the relative path.
- Mask columns Huffman-code through a canonical table in a 256B
  postings.bin prologue; lists under 16 postings stay raw.

Measured on linux (93,610 docs, 1.59GB text): FP 28.92 → 27.76, index
2.24GB → 1.42GB (ratio 1.42 → 0.90), FN=0, speedup 2.4x. Per file:
table 839 → 307MB (df=1 records drop their constant count byte behind a
per-block inline bitmap), postings 1356 → 1019MB, summaries 37 → 21MB.

Rejected with reasons:

- SELECTIVITY_REFINE_MULTIPLIER 4: admits refused wide/gap queries at
  90-100% FP for no wall win; stays 2.
- 40-bit table keys: hash32 collisions measured +31 candidates per 848k.
- Wider bucket masks (+1B/posting): gap-class grams occur 20+ times per
  file and saturate any bucket count; gap stayed ~90.5% after the
  hashing fix, so more buckets buy nothing there.
- The gap class is the structural floor of doc+bucket granularity; real
  positions would fix it and were rejected on size.

### Phase 5: residual classes

- Wide-class seams: boundary-byte-set representation so the seam contributes a
  constraint instead of nothing.
- Case-probe FPs: audit folded-supplement admission on case-sensitive plans.
- Revisit `tune()` constants (MAX_ALL_OF_GRAMS=3) and the 30% selectivity ceiling
  once block precision changes the cost model.

## Endline (2026-07-07, format frozen)

| metric | baseline | endline |
|---|---|---|
| linux aggregate FP | 38.06% | 27.76% |
| linux index bytes | 7.0GB (ratio 4.4) | 1.42GB (ratio 0.90) |
| linux suite speedup | 1.55x scan / 1.49x rg | 2.4x / 2.4x |
| guard (gitoxide) FP | 49.8% | 35.51% |
| guard index ratio | 5.12 | 1.13 |
| false negatives | 0 | 0 (suite-enforced) |

The seam-constant sweep (MAX_SEAM_CROSS 4096/8192, BOUNDARY_KEEP 12/16,
MAX_CONCAT_ALTERNATIVES 16, and the combination) found the shipped
constants optimal: every variant measured equal or worse.

What remains is the gap class (~90% FP, half the FP mass): occurrence
ordering and distance are invisible to doc+bucket granularity, and real
positions were rejected on size. That is the accepted floor of this
format. The single-digit aggregate target was not reachable inside the
size budget; 27.76% at 0.92x corpus is the frozen trade.

## Original targets

- Aggregate suite FP: 33.5% → single digits; gap and scatter classes → <10% each.
- Index size: ≤ 50% of corpus text bytes.
- Zero false negatives, always (suite-enforced).
- End-to-end suite speedups ≥ baseline (1.55x scan / 1.49x rg).
