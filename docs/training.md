# Training

The weight table is the one artifact left to produce. The index format,
the planner, and the crate surface froze at postings-v9; after the final
training run the table freezes too.

## What training produces

A 256×256 byte-pair count table, minted into a `WeightTable` binary. The
scanner keeps a gram when its border bigram weights beat every interior
bigram weight, so the table decides where gram borders land. Rare pairs
make selective grams.

The Python trainer is the production path: it streams the corpus
([training-data.md](training-data.md) is the data contract), counts
byte pairs through the GIL-free Rust counter, checkpoints, and mints a
table every terabyte. `sngram::learn::BigramCounter` serves local
experiments.

## Verdicts already measured

Do not relitigate these; the measurements are in
[fp-optimization-plan.md](fp-optimization-plan.md).

- **Mint untuned.** A sweep of boundary discounts 1/4/16/64 from the same
  counts lost to `Tuning::OFF` on aggregate false positives (32.50
  against 33.47/33.85/34.45). Discounting aligns gram borders with
  identifier vocabulary, which raises document frequency and weakens
  plans.
- **Scale has flat returns.** A 100GB table nearly matched the 12TB tier
  (32.50 against 31.82). Train to ~1TB for margin and stop; the
  distribution contract's 12TB cap is an upper bound, not a target.
- **The blend stays.** The Stack v2 / Software Heritage bucket
  distribution is the one the released tiers trained on.

## The run

```sh
cd train
uv sync
uv run sngram train --limit 1TB
uv run sngram train --mint-dir ./bins
uv run sngram inspect bins/*.bin
```

Credentials go in `train/.env`; the trainer needs a Hugging Face token
and Software Heritage S3 keys ([training-data.md](training-data.md)
lists them). Minting defaults to tuning off.

## Acceptance

Replace `crates/weights/data/final_weights.bin` with the minted table,
rebuild, and run both corpora:

```sh
just suite ~/ripos/linux
just guard
```

Gates, each against the frozen-table endline recorded in
[fp-optimization-plan.md](fp-optimization-plan.md):

- `false_negative_rows=0` on both corpora, unconditionally.
- Linux aggregate false positives at or below 27.76%.
- Guard false positives at or below 35.51%.
- `index_ratio` at or below 0.90 on Linux.
- Suite speedups at or above 2x.

A table that fails a gate does not ship; the embedded table only moves
forward. Record the new endline numbers next to the old ones before
committing the swap.
