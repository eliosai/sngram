# Final training run

The postings-v9 format and query pipeline are frozen. This is the spec for
the one production training run; after it, the weight table, the index
format, and the planner constants do not change.

## Verdicts already measured

- **Mint untuned.** The phase 4 sweep (discounts 1/4/16/64 from the same
  100GB counts) showed every tuning level losing to `Tuning::OFF`:
  aggregate FP 32.50 vs 33.47/33.85/34.45. Boundary discounting aligns
  gram borders with identifier vocabulary, raising df and weakening plans.
- **Corpus scale has flat FP returns.** The 100GB untuned table nearly
  matched the 12tb tier (32.50 vs 31.82 aggregate). Training past ~1TB
  buys almost nothing on FP; train to ~1TB for margin and stop.
- **The blend stays.** The existing production corpus blend is the one the
  released tiers were trained on; no evidence any reweighting helps FP.

## Run

```sh
cd python
uv sync
uv run sngram train --limit 1TB
uv run sngram train --mint-dir ./bins
uv run sngram inspect bins/*.bin
```

Mint with tuning off (the trainer default). The minted table becomes the
single production tier in `crates/weights/data`; bootstrap tiers and their
feature flags get removed in the same change.

## Acceptance

Rebuild and run the full suite on both corpora with the new table:

```sh
just suite ~/ripos/linux
just guard
```

Gates:

- `false_negative_rows=0` on both corpora
- linux aggregate FP at or below the frozen-table endline
- guard corpus FP at or below its endline
- `index_ratio` at or below the frozen-table endline
- suite wall speedups at or above the endline

Record the endline numbers in `docs/fp-optimization-plan.md` next to the
phase 6 results before swapping the table in.
