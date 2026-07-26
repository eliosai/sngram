# Training

Training produces a 256 by 256 byte-pair count distribution and serializes it
as a `WeightTable`. The scanner uses those weights to choose sparse gram
borders. Rare byte pairs create more selective grams.

The Python project in `train/` owns the production run. Rust's
`sngram::learn::BigramCounter` owns counting and table serialization. See
[training-data.md](training-data.md) for the corpus and distribution contract.

## Production Run

The run streams the `blend` corpus from the Hugging Face Hub with parallel
shard readers, dispatching to whichever family sits furthest below its
target share of counted bytes, counts every file once with vendored files
included, checkpoints every minute, and mints one final table when the
stream ends, stamped with a provenance record naming the corpus, its
roster fingerprint, the counted totals, and the realised mix.

```sh
cd train
uv sync
uv run pytest
uv run sngram train --mint-dir ./runs/r1
```

`--corpus stack-v3` runs the single-dataset path instead. The wire ceiling
to the Hub is 100 to 115 MB/s and snappy parquet expands about 3.3x, so
the counter sees a few hundred MB/s of decoded text with ten readers. The
shipped table took about ten hours to count 11.96 TB at a peak RSS of
about 2.2 GB. The weight table has 65,536 counters, so bigram frequencies
converge long before the corpus ends; a bounded run is first-class:

```sh
uv run sngram train --mint-dir ./smoke --shards 2 --no-dashboard
uv run sngram train --mint-dir ./smoke2 --limit 20GB --no-dashboard
```

`--shards N` consumes exactly the first N shards of every source, so a
killed and resumed bounded run reproduces the identical table. `--limit`
stops at the next batch boundary past the cap, so it overshoots on a fast
link. The rest of the surface is `--mint-dir`, `--corpus`, `--workers`,
`--checkpoint-every`, `--resume/--no-resume`, and
`--dashboard/--no-dashboard`.

Interrupt or kill the run at any point. Checkpoints are written at a
consistent quiesce, with the readers held still, so the counter and every
reader position describe one instant. Resume is byte-exact: the run picks
up at the byte it stopped on under the same mint directory and reproduces
the identical table. A checkpoint is bound to one dataset revision; a
republished dataset needs a fresh mint directory or `--no-resume`.

## Measured Context

Throughput collapses past about twelve reader threads from interpreter
contention; eight to ten readers saturate the link, which is what
`--workers` defaults to. Peak RSS holds near 2.2 GB across a full pass.
Earlier false-positive measurements showed small gains after roughly 1 TB.
Minting remains untuned because boundary discount sweeps performed worse
than `Tuning::OFF`.

## Acceptance

Inspect the minted table before embedding it:

```sh
uv run sngram inspect runs/r1/final_weights.bin
uv run sngram fs-validate runs/r1/final_weights.bin ~/repos/linux
```

After replacing the table in `crates/lib/data/`, rebuild and run the
optimization corpus plus at least one structurally different guard
corpus:

```sh
just eg release
just suite ~/repos/linux
just suite ~/repos/k8s
```

The release table must keep zero false negatives, meet the frozen
false-positive and index-ratio gates in
[fp-optimization-plan.md](fp-optimization-plan.md), and avoid a speed
regression on any corpus it is run against.
