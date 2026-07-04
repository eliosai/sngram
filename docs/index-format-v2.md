# Index format v2 — sentinels, folded twins, keys, tuned tables

What changed in the 2026-07 lib wave, what an integrator (eg, efs) must do to
use it, and why each piece kills a measured FP class. Everything here is
opt-in through `ScanOptions`/`IndexFormat`; defaults reproduce the old
behavior bit for bit (pinned by the differential suite).

## The pieces

| Piece | Index side | Query side | FP class it kills |
|---|---|---|---|
| Virtual line sentinels | `ScanOptions { line_sentinels: true }` — every document scans as `\n` + bytes + `\n` (a finalize tweak, not a hot-loop change) | `plan_query` with `IndexFormat { line_sentinels: true }`: edge `^`/`$` demand terminator-bridging grams (`\nfoo`, `foo\n`; CRLF dialects OR `\r`) | `$`-anchored literals: 99 %+ FP today (anchors were plan-invisible) |
| Folded twin space | `ScanOptions { fold: true }` — a second scan pass over the ASCII-folded stream; hashes salt with `HashKey::folded()`, so both spaces share one postings keyspace | `plan_query` with `IndexFormat { folded_space: true }`: effective-insensitive queries plan once in folded space (`PlannedQuery::space == Folded`) instead of exploding case variants | `-i`/smart-case: 80 %+ FP spikes from hundreds of variant OR branches |
| Valley-tuned tables | mint with `learn::Tuning` or re-mint existing bins via `scripts/mint-tuned-tables.py` (pure weight transform — no training run) | none — the table drives window selection on both sides identically | `sched_clock` trigram scatter (47 % FP: no bridging gram across `_`); the same discount on `\n`/`\r` makes sentinel grams exist |
| Keyed hashes | `ScanOptions { key: HashKey::new(secret) }` | hash plan grams with `Gram::hash_keyed` under the same key (folded space: `key.folded()`) | not FP — forgeability of the polynomial hash under hostile content |
| df hook | none | `DfStats` + `QueryPlan::tune` (And bags: rarest-first, stop-gram drops) + `estimate_candidates` for the scan-fallback cost model | numeric/hex/version classes selecting 46–84 % of the corpus at 97–99 % FP — routed to a scan instead |

## Rules that keep it sound

- Index build and query planning must agree on `ScanOptions`/`IndexFormat`;
  record the options next to the table fingerprint in the index manifest.
  Any change to options or table bytes is a global reindex event.
- The folded space is ASCII fold (byte-level, UTF-8-safe). Non-ASCII case
  variants stay expanded as branches inside the folded plan — sound, still
  collapses the dominant ASCII explosion. `plan_query` only selects the
  folded space when the query is effectively insensitive.
- Anchors strengthen plans only at pattern edges; interior impossible anchors
  (`foo$bar`) still prune to `None`.
- `tune` drops grams from And bags only (weakening is sound); Or bags are
  never thinned. Unseen grams are the provider's policy (top-K sampling ⇒
  unseen = rare).
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
