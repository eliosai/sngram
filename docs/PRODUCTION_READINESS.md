# Production readiness plan — eg + sngram

Compiled 2026-07-02 at HEAD `6fd9848`, after three adversarial review rounds of the
query pipeline and four in-depth production audits (query path on a 93,610-file
Linux checkout, index/storage with crash/concurrency/corruption experiments,
core library with publish/doc/test verification, product/CLI with output-parity
diffing). Every bullet cites how it was verified. Priorities: **P0** = ship
blocker, **P1** = required for production, **P2** = quality/polish.
Effort: S ≈ hours, M ≈ 1–2 days, L ≈ week+.

Current state for context: the query planner is converged — zero false negatives
across 151 corpus queries + 71 fresh adversarial queries vs `--no-index` ground
truth; ~330k soundness fuzz checks clean; every FP class a file-granular index
can express is optimized; plan-build bounded (~11 ms worst at the 4096-char
ceiling); warm lookup median 7.6 ms. What remains is below.

---


> **2026-07-03 measured results (Linux checkout, 93,610 files).** Index format
> v2 (sentinels + folded twins) + boundary-tuned tables (discount 16) + df
> cost model, verified by `eg-fn-check.sh`: **228 compared queries, 0 false
> negatives**. Against the forced-candidate floor (2,828 binary/high-entropy
> files, always verified by design):
> - `sched_clock` trigram scatter: was 47 % FP → **165 excess candidates on
>   175 matches (~100 % plan precision)**
> - `$`/`^` anchored literals: were 99 %+ FP → **non-matching candidates ≈
>   the floor** (anchor_end: 2,581 non-match on 8,488 matches)
> - `-i`/smart-case: was 80 %+ FP → **non-matching ≤ floor** (smart_spinlock:
>   2,808 on 3,466 matches; the folded space replaced variant explosions)
> - numeric/hex/version (97–99 % FP selecting 46–84 % of corpus): **routed to
>   scan by the estimate gate — zero index-path cost**
> - warm index open: 11.5 s full-body checksum → **34 ms sampled checksum**
> Remaining above-floor FP is the documented document-granular gap floor
> (`spin_lock.*spin_unlock` class) — line-scoped postings territory, accepted.

> **2026-07-03 lib wave (landed, all sngram-lib items).** The retrain bundle and
> core-library gaps below are closed in the lib crates; markers updated in
> place. Zero clippy warnings across sngram/sngram-types/sngram-weights
> --all-targets; 155 tests green. Landed: weights packaging + publish dry-run
> (bins in crates/weights/data, include list, 10tb feature, 6 dead bins
> pruned); table format v2 (validated version, embedded provenance,
> Truncated/InvalidVersion/InvalidProvenance errors); keyed gram hash
> (HashKey, mix(raw^key), folded-twin salt); mint-time valley tuning
> (learn::Tuning — separators _./-:, case seam, \n/\r discounted toward a
> floor; scripts/mint-tuned-tables.py re-mints existing bins without a
> training run); standard streaming scan format (virtual \n sentinels +
> ASCII-folded twin space) with differential pins;
> query planning (folded-space plans for -i/-S, terminator boundary
> grams for edge anchors, interior-anchor pruning preserved); DfStats +
> QueryPlan::estimate_candidates + tune (rarest-first, stop-gram drops from
> And bags only); streaming scan API; MSRV aligned at 1.96; README examples
> updated;
> sngram-weights narrowed to feature-selected `weights() -> WeightTable`;
> fp-queries.tsv +41 sentinel rows. NOT applied on purpose:
> #[non_exhaustive] on QueryPlan/PlanOptions —
> eg constructs/matches them exhaustively; that migration belongs to eg.

> **2026-07-03 eg wave 2 (landed, all eg-CLI items).** Second completion wave
> over the `eg` CLI: `cargo check`/`clippy`/`fmt -p eg --all-targets` clean.
> Landed: delta fold-into-base at 25 % of the base file count plus a
> MAX_DELTA_FILES cliff line under `--debug`; `--index-freshness=stat|hash`
> closing the same-stat silent-false-negative window (FNV over head+tail windows
> and length, stored in the manifest, stat fallback when absent); tantivy marked
> experimental (flag help + `--debug` notice, excluded from maintenance paths);
> indexed output parity — a single verify path buffers per file and releases in
> path order so `--` context separators appear in every thread config, and
> `--stats` documents its verified-candidate scope; unbanned
> `--files-without-match`, `--count --include-zero` (an empty-reader search
> synthesizes the zero line for ruled-out files), and `--json`; a first-time
> large-implicit-build stderr guardrail plus a `--debug` build-progress line
> every 20k files; `--index=verify` and `--index=repair` (section, checksum,
> manifest, and orphan checks, rebuild on fault); per-phase build progress and a
> documented single-threaded-merge rationale; binary-primary manifest with JSON
> gated behind `--debug`/`EG_INDEX_JSON_MANIFEST`; table.bin format v3 in
> `postings-v4` dropping the offset column (offset is the prefix sum
> reconstructed at open, ~50 % off table.bin); flags-only TSV rows in the fp/fn
> harness scripts; workspace-level indexing policy with invariant tests on the
> proven merge-join loops. Deliberately narrowed:
> merge stays single-threaded (documented), delta+varint postings deferred, and
> per-subtree/tombstone delta for deletions and renames is unchanged.

## P0 — ship blockers (all verified)

- [x] **`sngram-weights` cannot be published with any table feature** — FIXED 2026-07-03 (data/ move + include list; `cargo publish --dry-run --features 5tb` verifies) (its whole
  purpose). `include_bytes!("../../../bins/…")` escapes the crate dir, so the
  packaged tarball fails to build — reproduced with
  `cargo publish -p sngram-weights --dry-run --features 5tb`. Any downstream
  `features=["5tb"]` against crates.io fails to compile. Move bins into
  `crates/weights/data/`, add `include=[...]`, gate each feature with a dry-run
  in CI. *(M)*
- [ ] **Parallel verify output is non-deterministic.** Same query, 10 runs, 10
  different orderings (content identical) whenever `threads>1 && candidates>=4096`
  — workers print per-file buffers in completion order
  (`crates/eg/src/index/mod.rs` verify_candidates_parallel). Breaks pipelines,
  goldens, reproducibility. Buffer per file and emit in path order. *(M)*
- [ ] **Unindexable queries hard-error instead of falling back to a scan.**
  `eg a`, `eg .`, `eg '\w+'`, `eg ''`, `-v`, stdin pipes, `--pre`, `-z`,
  `--encoding`, PCRE2 — all exit 2 with "use --no-index". ripgrep runs all of
  them. Default should be transparent fallback to the scan path (stderr note
  under `--debug`), with `--index=require` for strict mode. Includes
  **stdin**: `echo foo | eg foo` must work. *(M)*
- [ ] **Read-only corpora cannot be searched at all** — eg always writes
  `<corpus>/.eg` and dies on permission-denied (verified). Add an index-location
  override flag plus an XDG cache fallback (`~/.cache/eg/<corpus-hash>/`). *(M)*
- [ ] **No durability or mutual exclusion in the index layer** (all reproduced):
  - Zero fsync anywhere; the manifest (commit point) can survive power loss over
    torn `table.bin`/`postings.bin`. fsync data → then manifest → then dir. *(S)*
  - No lock file: concurrent runs race — 2 of 60 concurrent queries died with
    ENOTEMPTY during a rebuild; two parallel `--index=rebuild` → one exits 2.
    Advisory build lock + read snapshots. *(M)*
  - Rebuild is destroy-then-build-in-place (`remove_dir_all` first): a crash
    mid-rebuild permanently loses the old index, and there is a multi-second
    (4.7 s at 50k files) window with no index at all. Build to temp dir, swap
    via rename / generation dirs. *(M)*
  - No checksums/magic/version header on `table.bin`/`postings.bin`: injected
    corruption returned results **silently** — a silent-false-negative vector.
    Add file headers (magic, version, count, checksum). *(S)*
  - Corrupt index bricks search (hard error, no self-heal) until manual
    `--index=rebuild`; auto-rebuild on structural/checksum failure. *(S)*
- [x] (lib side) **A ≥4 GiB file panics the whole index build** — fixed by the
  `BufRead` scan API; eg wiring in the P0 CLI wave. Oversized files should be
  streamed, skipped, or forced as candidates according to `--max-filesize`.
  *(M)*

---

## Query path

> **2026-07-02 final-wave update.** Per Josh: the eg CLI's constant per-query
> overhead is bench-harness territory — it may be slow; those bullets are
> deprioritized to P2 and the daemon idea is optional. The sngram-lib items
> below marked DONE landed in the final wave (commits 6aecb68, 9bacaf8):
>
> - DONE — exact bounded-rep expansion above the ×4 cap (`h{3,5}i` now plans
>   `hhh`+`hhi`; guard on projected set size keeps `[0-9a-f]{16}` folding).
>   Note: `x{5}` alone is unfixable by ANY gram scheme — uniform runs collapse
>   to a single `"xxx"` gram (verified `index_grams("xxxxx") == {"xxx"}`).
> - DONE — `plus_base` seam tracking: `mem+set` requires `memset|mmset`
>   windows, `rea+d_lock` requires `read_lock|aad_lock` (proven identities:
>   suffix(X·E+) = (suffix(X) ∪ E) × E, prefix symmetric).
> - DONE — F6 wide-class boundary byte-sets: `\p{Greek}term_var` now plans
>   ~50 continuation-byte windows (was pure `cover(term_var)`); UTF-8 byte
>   derivation proven exhaustive over all 1.1M scalars; capped at 64
>   bytes/side, `.`-like classes unchanged.
> - REJECTED (with proof) — double adjacent enumerated classes
>   (`read[a-zA-Z][a-zA-Z]lock`): contiguity needs an irreducible 52×52 =
>   2,704-branch OR > the 2,048 flush cap; over budget by design. Floor.
> - DONE — independent soundness certification of all post-cad48c6 planner
>   mechanisms: 520,027 verified matches across dual oracles
>   (regex-automata on the planner's exact HIR + grep-regex built exactly as
>   production), ~248.6M-document sweep proving every None plan empty, all
>   plans ≤ 5,211 grams, harness mutation-validated. Verdict: NOTHING-LEFT.


- [ ] **P2 (bench-only per Josh) Kill the ~180–235 ms constant per-query overhead** — it dominates
  interactive latency (measured: a no-match literal costs 235 ms total with
  0.07 ms verify). Breakdown: `git status` subprocess 90–111 ms; rebuilding all
  93,610 `CurrentFile` structs per query ~60 ms; binary manifest decode ~26 ms;
  postings open+mmap ~37 ms. In order of value:
  - [ ] Daemon / resident mode (hot manifest + snapshot + mmaps). *(L)*
  - [ ] Lazy snapshot materialization — resolve paths only for candidates. *(M)*
  - [ ] Cache/short-circuit git status (HEAD oid + index mtime), or async. *(M)*
  - [ ] mmap the binary manifest instead of decode-to-Vec. *(M)*
- [x] (lib side) **P1 Cost model / scan fallback for low-selectivity plans.** — `estimate_candidates` from df priors; eg fallback wiring pending Numeric/hex/
  version classes select 46–84% of the corpus at 97–99% FP
  (`v[0-9]+\.[0-9]+\.[0-9]+` → 78,583 candidates / 231 real) — strictly more
  work than a plain scan. Posting-list lengths (df) are already in the index:
  estimate candidate cardinality pre-lookup; above ~N% of corpus, use the
  parallel walk instead. *(M–L, no format change)*
- [x] (lib side) **P1 Feed document frequency into gram selection.** — `DfStats` + `QueryPlan::tune` (And bags: rarest-first, stop drops) + `estimate_candidates`; eg wiring pending The planner is
  corpus-agnostic; df is only used to order AND intersections. Letting
  `and_grams`/`branch_covers` prefer rare covers and drop ubiquitous grams from
  OR bags is the principled fix for mixed literal+class patterns. *(L, no
  format change)*
- [ ] **P2 Parallel verify scaling**: good to ~4×, plateaus/regresses at j16
  (mutex'd VecDeque + per-candidate locked prints). Chunked/work-stealing queue,
  per-thread buffers, cap workers ~8. Do NOT lower the 4096 threshold (measured
  harmful). *(S–M)*
- [ ] **P2 Dedup minimal⊂maximal literal-cover redundancy** (cost-only). *(S)*
- [ ] **P2 Plan-quality regression harness**: candidate-count / plan-gram-count
  ceilings per query class, so silent FP regressions get caught (soundness and
  shape tests don't). *(M)*
- [x] **P2 Audit indexing slices** — LANDED 2026-07-04: workspace-level policy
  plus focused invariant tests on scanner spans, truncation/spill paths, and
  sorted merge tails. *(S)*
- [x] **P2 Harness tweak** — LANDED 2026-07-03: `eg-fp-rates.sh` and
  `eg-fn-check.sh` accept flags-only TSV rows (empty pattern column), so
  multi-`-e` invocations run without a positional pattern. *(S)*

## Index and storage

(P0 durability/locking/checksum items listed above.)

- [ ] **P1 Delta/incremental gaps**:
  - Full-rebuild cliff at `MAX_DELTA_FILES=4096` (99 ms → 883 ms measured);
    deletions and renames always force full rebuilds. Per-subtree manifests or
    tombstones + ord stability. *(M–L)* — cliff now surfaced in `--debug` when
    hit; per-subtree/tombstone work is unchanged.
  - [x] Delta base never compacts — FIXED 2026-07-03: `delta_should_fold` folds
    the delta into a fresh base once it exceeds 25 % of the base file count
    (`DELTA_FOLD_PCT`), before stale postings dominate as FP candidates. *(M)*
  - [x] Orphan `runs/` and torn delta files — swept on open (`sweep_orphans`).
- [x] **P1 Freshness is mtime+ctime+len only** — FIXED 2026-07-03:
  `--index-freshness=hash` (default `stat`) hashes the length plus head and tail
  windows, stored per file in the manifest, closing the same-stat
  silent-false-negative window; stat comparison is the fallback when a hash is
  absent. *(M)*
- [ ] **P1 Binary/high-entropy files bloat the index ~38×** (400 KB random →
  15.5 MB table) and are search-skipped by ripgrep anyway. Apply binary
  detection (or unique-gram-ratio cap) at index time. *(M)* — landed in the P0
  wave (`classify::is_binary`/`is_high_entropy`), forced-candidate not gram-indexed.
- [x] **P1 Tantivy backend: gate or invest** — GATED 2026-07-03: marked
  experimental in the `--index-backend` help, warned under `--debug` when
  selected, and excluded from the maintenance (`--index=verify`/`repair`) paths.
  No parity tests by decision. *(M)*
- [ ] **P1 `.eg` placement and hygiene**: index dir depends on cwd for
  multi-path invocations (different cwd → different index); no auto
  `.eg/.gitignore` (repos become dirty; a multi-hundred-MB index can get
  committed). Stable index home + self-ignoring dir + docs. *(S)*
- [x] **P2 `--index=verify` / `--index=repair` commands** — LANDED 2026-07-03:
  verify checks manifest presence and table compatibility, base and delta
  section headers and sampled checksums, delta completeness, and leftover
  run/staging artifacts, reporting per-check and exiting 0/1; repair rebuilds on
  any fault. Postings backend only (tantivy is experimental). *(S)*
- [x] **P2 Build observability** — LANDED 2026-07-03: a `--debug` progress line
  every 20k files scanned plus a per-phase (scan-done, merging-N-runs) marker.
  Merge stays single-threaded by design: it streams one monotonic key sequence
  into two append-only sections (I/O-bound, not CPU-bound), and a per-shard
  parallel merge would need the scan phase to range-partition runs by hash; the
  rationale is documented on `merge_runs`. *(M)*
- [x] **P2 Manifest scalability**: binary-primary — LANDED 2026-07-03: the
  compact binary manifest is the commit point and the JSON manifest is only
  written under `--debug` or `EG_INDEX_JSON_MANIFEST`, avoiding the full-corpus
  JSON rewrite every build; reads accept either form. Per-query stat-all-files
  freshness (incremental) is unchanged. *(M)*
- [ ] **P2 Index size (currently 5–6× content)** — cheap wins first:
  - [x] Drop the redundant `offset` column from `table.bin` — LANDED 2026-07-03:
    section/table format v3 in `postings-v4`, records are `hash`+`len` (12 B,
    was 24), the offset is the prefix sum reconstructed once at segment open
    (`prefix_offsets`): **~50 % off table.bin**. *(S)*
  - Delta+varint (or roaring for dense lists) postings: est. 40–60% off
    `postings.bin`; combined ≈ 2× smaller index. *(M)* — deferred (not clean
    alongside the format bump; revisit as a separate change).

### Index-shape roadmap (design decisions, biggest remaining FP/latency levers)

- [ ] **Line-scoped postings or per-file line blooms** — kills the largest FP
  floor (gap queries: `sched.*clock` 86%, `$`-anchored literals 99%+ FP;
  inherently invisible to a file-granular index). Sidecar file keeps old
  indexes valid. *(L)*
- [x] **Case-folded gram field** — landed as the standard folded twin space
  (`scan` emits it; `query` selects it; same postings keyspace,
  FOLD-salted hashes) — collapses `-i`/smart-case OR-plans (hundreds
  of variant branches) into single folded lookups; ~+40–60% table size or
  replace-with-folded tradeoff. *(M)*
- [ ] **Positional postings (codesearch-style)** — adjacency/order pruning;
  2–4× postings growth; only worth it if verify becomes the bottleneck (today
  it is not: 3 ms/500 candidates). *(L, likely defer)*
- [x] **Weight-table retraining with valley objectives at identifier
  boundaries** — `learn::Tuning` + `scripts/mint-tuned-tables.py` (pure weight transform, no training run); gate via eg-fp-rates.sh (`_`, case transitions, `.`, `/`) — addresses the trigram-scatter
  precision floor (`sched_clock` 47% FP: no bridging valley gram on the current
  table). No format change but a fingerprint bump = global rebuild event. Gate
  quality with `scripts/eg-fp-rates.sh` as the regression metric. *(M–L)*

## Core library

- [ ] **P1 CI from zero** (none exists): test all feature combos, clippy
  `-D warnings` (currently fails on 17 test-code lints — fix those),
  fmt, rustdoc `-D warnings`, MSRV job, per-feature `publish --dry-run`,
  bench smoke with committed criterion baselines. *(M)*
- [x] **P1 MSRV** — aligned at 1.96 (declared floor is the tested floor); CI enforcement with the CI item
- [x] (partial) **P1 Pre-1.0 API contract** — `sngram-weights` now exposes a single feature-selected `weights() -> WeightTable`; QueryError already uses thiserror + non_exhaustive; QueryPlan/PlanOptions left constructible (eg breaks otherwise — eg-side migration): `#[non_exhaustive]` on `QueryPlan`,
  `PlanOptions` (or builder);
  document the Pattern vs `query_with` duality (or add a Pattern+options
  overload to avoid the re-parse). *(S)*
- [x] **P1 Document the prefilter contract + hash policy** — documented in hashing.rs; keyed variant landed (`HashKey`): 64-bit gram-hash
  collisions cause false positives only (verified end-to-end); negligible at
  scale (~0.03 expected colliding pairs at 1B grams) but the polynomial hash is
  adversarially forgeable — document, and offer a keyed variant for
  untrusted-content deployments. *(S–M)*
- [x] **P1 Fix `crates/lib/README.md` examples** — fixed + wired as doctests (`ReadmeDoctests`) (pass `&table`; they don't
  compile) and wire READMEs/examples into doctests. *(S)*
- [ ] **P2 Productionize the fuzzers** as cargo-fuzz targets: scan-vs-reference,
  streaming chunking, plan soundness vs regex oracle, table/pattern/hash
  panic hunting. Add proptest for StringSet + Query algebra invariants. *(M)*
- [x] **P2 Weight-table provenance** — v2 format embeds provenance; version validated in from_bytes: bins carry no corpus/date/commit record;
  `WeightTable.version` is loaded but never validated (always 1). Provenance
  manifest per bin + documented retrain workflow + version enforcement. *(M)*
- [ ] **P2 Repo hygiene**: prune 7 unused weight bins (~2 MB dead weight) or
  gate them; commit `Cargo.lock` (currently ignored via `**/*.lock`); replace
  the kernel-derived `.gitignore`; document `sngram-types` public items (drop
  the blanket `missing_docs` allow); remove the two provably-infallible
  `.expect()`s; fix `TableError::InvalidMagic` misuse for truncated headers;
  add async `index_reader` coverage. *(S each)*

## Product, CLI, and operations

(P0 fallback/stdin/read-only/determinism items listed above.)

- [x] **P1 Indexed output parity with ripgrep** — LANDED 2026-07-03:
  - [x] File ordering — candidates verify in path order (`ordered_candidates`).
  - [x] Context separators (`--` between files with `-C`) — the serial and
    parallel verify paths are unified onto one per-file-buffer + reorder-buffer
    emission, so the buffer writer inserts separators between adjacent printed
    files in every thread config, not just `-j1`. *(M)*
  - [x] `--stats` — the flag help documents that indexed mode reports
    verified-candidate (not corpus-wide) files/bytes searched. *(S)*
- [x] **P1 Unban what the index can serve** — LANDED 2026-07-03:
  `--files-without-match` and `--count --include-zero` iterate the whole corpus
  in path order, searching candidates for real and driving the printer with an
  empty reader for index-ruled-out files (the exact zero/without-match line at no
  I/O cost); `--json` is unbanned (printer-only, determinism already fixed). *(M)*
- [x] **P1 Guardrails** — LANDED 2026-07-03: a first-time implicit build over
  `$HOME` or a >100k-file tree prints a one-line stderr warning naming
  `--no-index` and `--index-dir` before building; a `--debug` progress line
  reports every 20k files scanned during a build. *(M)*
- [ ] **P2 Branding**: "elgrep (eg)" vs crate `eg` vs README that only covers
  the library — pick a name, write a real CLI README, fix the
  `--index-backend` help copy-paste ("tantivy uses eg's compact mmap-backed
  postings index" should say "postings"). *(S)*
- [ ] **P2 Release engineering**: no CHANGELOG, no packaging/cargo-dist, no
  cross-platform CI (mmap/rename/locking semantics unvalidated on
  Windows/macOS), no reproducible-build story for embedded tables. *(M–L)*

## Corpus/harness additions (adopt into `scripts/fp-queries.tsv`)

- [x] Numeric/hex/version sentinels — added 2026-07-03: `ver_semver`, `rep_ip_octet`, `hex_addr16`,
  `num_mask`, `num_bit_shift` (the largest non-floor FP class).
- [x] Bounded-rep cap sentinels — added: `x{5}`, `ab{5}cd`, `\w{4,8}_ops`.
- [x] CRLF/anchor floor sentinels — added: `EXPORT_SYMBOL$ --crlf`, `^#define CONFIG --crlf`.
- [x] Deep alternations — added: `GFP_(KERNEL|ATOMIC|NOWAIT|NOFS|NOIO|USER)`,
  `IRQF_(...)`, `((un)?register|(de)?activate)_netdev`.
- [x] Realistic dev greps — added: `int \(\*\w+\)\(`, `static const struct \w+_operations`,
  `skb->data`, `\(struct sock \*\)`, `%pS`, `0x%08x`, `pr_err\("%s:`,
  `-E(INVAL|NOMEM|BUSY|AGAIN)`, `ERR_PTR\(-E`, `SPDX-License-Identifier: GPL`,
  `Signed-off-by:`.
- [x] Smart-case + non-ASCII — added: `spin_lock_irqsave -S`, `netif_receive_skb_core -S`,
  `TaskStruct -S`, `µs`, `中文`, `include/linux/netdevice\.h`.
- [x] Gap floor sentinels — added: `spin_lock.*spin_unlock`, `static.*return -E`.

## Explicitly accepted floors (documented, not TODOs)

- Document-granular gap queries (`sched.*clock` ~86% per-candidate FP; absolute
  narrowing still ~50:1) — only fixable by line-scoped indexing (roadmap above).
- Trigram scatter on flat-weight text (`sched_clock` 47%) — weight-table
  retraining territory (roadmap above).
- `(ab){2,}c`-style cases that are unprovable under the crc32 *test* table only.
- Short patterns (<3 bytes), `.*`, `\w+`, `kfree|ab` — genuinely unindexable;
  correct bans (to become scan-fallbacks per the P0 item).
