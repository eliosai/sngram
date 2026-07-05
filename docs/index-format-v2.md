# Index format v2 — sentinels, folded twins, keys, tuned tables

What changed in the 2026-07 lib wave, what an integrator (eg, efs) must do to
use it, and why each piece kills a measured FP class. These pieces are now the
standard `sngram::scan`/`sngram::query` index format: scan emits primary and
folded gram spaces from one `BufRead` stream, and query selects the matching
space for the verifier semantics.

## The pieces

| Piece | Index side | Query side | FP class it kills |
|---|---|---|---|
| Virtual line sentinels | `scan` brackets every document as `\n` + bytes + `\n` | `query` demands terminator-bridging grams for edge `^`/`$` anchors (`\nfoo`, `foo\n`; CRLF dialects OR `\r`) | `$`-anchored literals: 99 %+ FP today (anchors were plan-invisible) |
| Folded twin space | `scan` emits primary bytes and an ASCII-folded twin stream; folded hashes are salted with `HashKey::folded()`, so both spaces share one postings keyspace | effective-insensitive queries plan once in folded space (`QueryPlan::space() == Folded`) instead of exploding case variants | `-i`/smart-case: 80 %+ FP spikes from hundreds of variant OR branches |
| Valley-tuned tables | mint with `learn::Tuning` or re-mint existing bins via `scripts/mint-tuned-tables.py` (pure weight transform — no training run) | none — the table drives window selection on both sides identically | `sched_clock` trigram scatter (47 % FP: no bridging gram across `_`); the same discount on `\n`/`\r` makes sentinel grams exist |
| df hook | none | space-aware `DfStats` + `QueryPlan::tune` (And bags: rarest-first, stop-gram drops) + `estimate_candidates` for the scan-fallback cost model | numeric/hex/version classes selecting 46–84 % of the corpus at 97–99 % FP — routed to a scan instead |

## Rules that keep it sound

- Index build and query planning must use the same table bytes and the standard
  scan/query format; record the table fingerprint in the index manifest. Any
  change to table bytes is a global reindex event.
- The folded space is ASCII fold (byte-level, UTF-8-safe). Non-ASCII case
  variants stay expanded as branches inside the folded plan — sound, still
  collapses the dominant ASCII explosion. `query` only selects the folded space
  when the query is effectively insensitive.
- Anchors strengthen plans only at pattern edges; interior impossible anchors
  (`foo$bar`) still prune to `None`.
- `tune` drops grams from And bags only (weakening is sound); Or bags are
  never thinned. `DfStats::doc_count` receives the plan's `GramSpace`, so folded
  and primary posting frequencies do not need an out-of-band provider
  convention. Unseen grams are the provider's policy (top-K sampling ⇒ unseen =
  rare).
- Table format v2 = v1 header (version 2) + weights + u16-length provenance
  tail; `WeightTable::from_bytes` validates version, checksum, and
  provenance. v1 loads unchanged.

## The mint

`scripts/mint-tuned-tables.py --discount 16 crates/weights/data/*.bin` emits
tuned v2 tables to `/tmp/sngram-mint` (or `--in-place` for the official
re-mint). The discount is the tunable; gate any change with
`scripts/eg-fp-rates.sh` over `scripts/fp-queries.tsv` (watch `sched_clock`,
`anchor_*`, `smart_*`, precision_pct) plus the differential and soundness
suites.
