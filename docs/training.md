# Training

Training produces a 256 by 256 byte-pair count distribution and serializes it
as a `WeightTable`. The scanner uses those weights to choose sparse gram
borders. Rare byte pairs create more selective grams.

The Python project in `train/` owns the production run. Rust's
`sngram::learn::BigramCounter` owns counting and table serialization. See
[training-data.md](training-data.md) for the corpus and distribution contract.

## Production Run

The run streams The Stack v3 from the Hugging Face Hub with parallel shard
readers, counts every non-vendored file once, checkpoints every minute, and
mints one final table when the stream ends, stamped with a provenance record
naming `stack-v3`, the resolved dataset revision, the counted totals, and
the realised language mix.

```sh
cd train
uv sync
uv run pytest
uv run sngram train --mint-dir ./runs/r1
```

The wire ceiling to the Hub is about 115 MB/s and parquet expands about
3.4x, so the counter sees roughly 250 to 375 MB/s of decoded source text
with ten readers. The full 4.71 TB pass is roughly 11 to 16 hours. The
weight table has 65,536 counters, so bigram frequencies converge long
before the corpus ends; a bounded run is first-class:

```sh
uv run sngram train --mint-dir ./smoke --shards 10 --no-dashboard
uv run sngram train --mint-dir ./smoke2 --limit 20GB --no-dashboard
```

`--shards N` consumes exactly the first N shards, so a killed and resumed
bounded run reproduces the identical table. `--limit` stops at the next
batch boundary past the cap, so it overshoots on a fast link.

Interrupt or kill the run at any point; it resumes from the checkpoint
under the same mint directory and reproduces the identical table. A
checkpoint is bound to one dataset revision; a republished dataset needs a
fresh mint directory or `--no-resume`.

## Measured Context

Throughput collapses past about twelve reader threads from interpreter
contention; eight to ten readers saturate the link. Earlier false-positive
measurements showed small gains after roughly 1 TB. Minting remains untuned
because boundary discount sweeps performed worse than `Tuning::OFF`.

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
