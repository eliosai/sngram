# Training

Training produces a 256 by 256 byte-pair count distribution and serializes it
as a `WeightTable`. The scanner uses those weights to choose sparse gram
borders. Rare byte pairs create more selective grams.

The Python project in `train/` owns the production run. Rust's
`sngram::learn::BigramCounter` owns counting and table serialization. See
[training-data.md](training-data.md) for the corpus and distribution contract.

## Production Run

The published dataset is the exact corpus, so the run streams it row by
row and consumes every object once: 5.11 TB effective. It checkpoints
every minute and mints one final table when the stream ends, stamped with
a provenance record naming the corpus revision and the counted totals.

```sh
cd train
uv sync
uv run pytest
uv run sngram train --mint-dir ./runs/r1
```

Corpus rows stream from the Hugging Face Hub while content streams from
the public Software Heritage bucket with anonymous bounded reads; nothing
is prefetched. A 20-core machine with 95 ms of latency to the bucket
sustains 85 to 90 MB/s of effective counting, which puts the full run
around sixteen hours.

Use `--limit` for a smoke run without changing the production default:

```sh
uv run sngram train --mint-dir ./smoke --limit 1GB --no-dashboard
```

Interrupt or kill the run at any point; it resumes from the checkpoint
under the same mint directory and reproduces the identical final table. A
checkpoint is bound to one corpus revision; a republished corpus needs a
fresh mint directory.

## Measured Context

Earlier false-positive measurements showed small gains after roughly 1 TB.
The full-corpus run prioritizes stable coverage across every live format.
Minting remains untuned because boundary discount sweeps performed worse
than `Tuning::OFF`.

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

The release table must keep zero false negatives, meet the frozen
false-positive and index-ratio gates in
[fp-optimization-plan.md](fp-optimization-plan.md), and avoid a speed
regression on either corpus.
