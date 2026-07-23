# Training

Training produces a 256 by 256 byte-pair count distribution and serializes it as
a `WeightTable`. The scanner uses those weights to choose sparse gram borders.
Rare byte pairs create more selective grams.

The Python project in `train/` owns the production run. Rust's
`sngram::learn::BigramCounter` owns counting and table serialization. See
[training-data.md](training-data.md) for the corpus and distribution contract.

## Canonical Run

The canonical target is 10 TB of effective bytes. It mints balanced tables at
100 GB, 500 GB, and each terabyte through 10 TB.

```sh
cd train
uv sync
uv run pytest
uv run sngram train --mint-dir ./bins
```

Use `--limit` for a smoke run without changing the production default:

```sh
uv run sngram train --mint-dir ./smoke --limit 1GB --no-dashboard
```

Resume uses the manifest and checkpoint under the same mint directory. Do not
reuse that directory after changing the target, pinned revision, area roster, or
format assignment.

## Measured Context

Earlier false-positive measurements showed small gains after roughly 1 TB. The
10 TB canonical run prioritizes stable coverage across every live format and
produces comparable balanced tiers. Minting remains untuned because boundary
discount sweeps performed worse than `Tuning::OFF`.

## Acceptance

Inspect the minted table before embedding it:

```sh
uv run sngram inspect runs/r1/final_weights.bin
uv run sngram fs-validate runs/r1/final_weights.bin ~/ripos/linux
```

After replacing the table in `crates/lib/data/`, rebuild and run both
corpora:

```sh
just suite ~/ripos/linux
just guard
```

The release table must keep zero false negatives, meet the frozen false-positive
and index-ratio gates in [fp-optimization-plan.md](fp-optimization-plan.md), and
avoid a speed regression on either corpus.
