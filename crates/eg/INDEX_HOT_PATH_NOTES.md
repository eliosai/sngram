# eg index hot path notes

Linux reference: `/home/josh/ripos/linux`, `postings-v3`, 93,610 files, 5.3 GB index.

## Current hot-path timings

- First run after binary sidecar patch still reads `manifest.json`: 49.2 ms.
- Hot run reads `manifest.bin`: 16.2-19.9 ms.
- Git freshness, when counted, is still 121.8-124.9 ms.
- Sparse n-gram lookup is already small for normal queries:
  - `Everything`: 365 candidates, 0.55 ms lookup, 4.85 ms verify.
  - `sched_clock`: 333 candidates, 0.66 ms lookup, 4.70 ms verify.
  - long no-match: 0 candidates, 0.47 ms hot lookup; first cold run was 14.94 ms due mmap page faults.

## Manifest bottleneck

The JSON bottleneck is partly solved, but the owned `Manifest` model is still too expensive. The sidecar removes serde JSON work, yet it still reads/decodes the whole file table into owned `String`s and then the freshness path builds a full `CurrentSnapshot` with `PathBuf`s for every file.

Target design:

- Store an mmap-friendly archived manifest: fixed header, fixed file records, fixed dir records, string blob.
- Read compatibility fields from the header only. This should be sub-millisecond.
- Keep file paths as borrowed byte/string slices from the mmap.
- Materialize `PathBuf`/`Haystack` only for candidate ordinals and changed ordinals.
- Replace per-run path `HashMap` construction with a persisted path-hash table, sorted by hash with collision verification.
- Split freshness from query execution so a trusted-clean or watcher-backed run does not allocate all files.

`rkyv` can do this, but a custom layout is also reasonable because the format is tiny and needs stable mmap access more than general serialization.

## Query pipeline issues to revisit

- `sched.*clock`: high FP is expected with the current plan because wildcard distance/adjacency is not represented; it only requires sparse grams from both literal islands.
- `sched[_-]clock`: high FP is a planner weakness. The class plan is much weaker than explicit alternation `sched_clock|sched-clock`, even though the language is nearly the same.
- `max_file_size -i`: high FP comes from Unicode case expansion producing a huge OR plan. A better sparse table will not fix this by itself; we need ASCII-only literal folding where legal, or an index-side folded domain.
- `sched_clock`: literal plan is sound but weak (`"_cloc" "d_c" "ock" "sched_"`). Strengthening literal covers must happen in `sngram` with proof tests, not by AND-ing arbitrary extra grams in `eg`.

## Planner research plan

Reference: Google Code Search's trigram planner uses exact/prefix/suffix/match
regex summaries and boolean AND/OR trigram queries. That design transfers, but
the current `sngram` port still has trigram-shaped assumptions.

General issues to fix before table tuning:

- Sparse-native context: `BOUNDARY_CTX = 2` is trigram-specific. Sparse grams can
  be longer, so prefix/suffix retention and seam mining need a budgeted
  sparse-cover policy instead of a fixed two-byte stub.
- Branch precision: small classes and equivalent alternations should compile to
  similarly precise plans. `sched[_-]clock` should not be much broader than
  `sched_clock|sched-clock`.
- Bounded products: adjacent small classes/literals should be expanded while the
  product is under a plan-size/cost budget, then degraded only when necessary.
- Repetition precision: `x{n,m}` and small `x+` cases should retain guaranteed
  repeated structure when bounded. Current demotion treats most `min > 0`
  repeats as "one or more" and loses useful grams.
- Case folding: Unicode case expansion can create broad ORs. Add a general
  fold strategy: ASCII folded literal domain where semantics allow it, Unicode
  fallback with explicit cost caps, and no narrowing that could miss matches.
- Cost-aware simplification: do not simplify only by node count. Prefer plans
  estimated to have fewer candidates using posting cardinalities or table
  weights, while preserving the no-false-negative invariant.
- Optional positional layer: wildcard gaps and boundaries cannot reliably reach
  low FP with document-level gram presence alone. If the <20% target must cover
  gap-heavy regexes, add a second-stage order/distance filter or positional
  postings.

Planner-quality benchmark cases:

- Literals: `max_file_size`, `sched_clock`, `content_ngrams`, `WeightTable`.
- Identifier gaps: `max_\w+_size`, `sched_\w+`, `[a-z]+_file_size`.
- Shared affix alternation: `(max|min)_file_size`, `sched_(clock|timer|deadline)`.
- Equivalent class/alt: `sched[_-]clock` vs `sched_clock|sched-clock`.
- Case modes: `(?i)max_file_size`, `(?i)WeightTable`, `(?i)straße`.
- Small classes: `ab[cd]ef`, `foo[α-γ]bar`, `[._-]config`.
- Wide/negated classes: `ab[^cde]f`, `name\s*=\s*[^\n]+`, `"[^"]+"`.
- Repetition/gaps: `a+hello`, `(abc)+`, `foo.{0,20}bar`, `foo.*bar`.
- Optional pieces: `(?:get_)?max_file_size`, `colou?r`, `async\s+fn`.
- Boundaries/anchors: `\bmax_file_size\b`, `^use crate::`, `^fn main`.
- Punctuation code: `#\[derive\(`, `\.unwrap\(\)`, `-> Result<`, `::new\(`.
- Structured docs/config: `apiVersion:\s+apps/v1`, `"max_file_size"\s*:`,
  `FROM\s+\w+:\w+`, `SELECT .* FROM`.
- Multilingual: `こんにちは`, `错误代码`, `变量_name`, `\p{Han}+`, `\p{Greek}+`.

Bench target: for each query and corpus family, report regex hits, prefilter
hits, false positives, FPR over non-matching docs, precision, plan shape,
AND/OR width, gram count, lookup time, and verify time. Keep `sched_clock` and
`max_file_size` only as representatives of broader classes, not tuning targets.
