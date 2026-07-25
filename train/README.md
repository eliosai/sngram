# sngram-train

The trainer that mints sngram weight tables.

It trains on The Stack v3 (`HuggingFaceCode/stack-v3-train`), reading
everything from the Hugging Face Hub and nothing else. Parallel
fetchers spool the parquet shards to disk while a bounded decoder
reads only the file content columns, vendored files included, and
counts byte pairs through the Rust core with the GIL released. The
run checkpoints every minute at a consistent pause, resumes
byte-exactly after a kill, retries transient network failures, and
mints one provenance-stamped `final_weights.bin` when the stream ends.
Nothing outlives the run on disk except the checkpoint, the event log,
and the final table; in-flight shards spool under `.spool` in the mint
directory and are deleted as they finish.

This project depends on the `sngram` library by path and is not
published. The library it drives lives in
[crates/python](../crates/python).

## Running

```sh
uv sync
uv run sngram train --shards 10                # smoke run, ~20 GB decoded
uv run sngram train --mint-dir ./runs/r1       # full corpus
uv run sngram inspect runs/r1/final_weights.bin
```

The corpus is 8196 parquet shards, 4.71 TB on the wire, 15.9 TB of
decoded source text over 172.9M repositories. One row is one
repository; the trainer takes each repository's natural file mix, so
the trained distribution matches real source trees. A full pass takes
roughly 11 to 16 hours at the measured 250 to 375 MB/s of decoded
text. `--shards N` bounds a run to the first N shards and `--limit`
caps decoded bytes; both mint a table. Reading the dataset needs an
`HF_TOKEN` in the environment or `train/.env`. Override the dataset
with `SNGRAM_DATASET_REPO`.

Interrupt or kill the run at any point; it resumes from the checkpoint
under the same mint directory and reproduces the identical table. The
live dashboard shows decoded throughput, progress with ETA, the
realised language mix, and errors; every event also lands in a JSONL
log.

The corpus contract lives in
[docs/training-data.md](../docs/training-data.md) and the production
run in [docs/training.md](../docs/training.md).

## Layout

`sngram_train/` holds the pipeline: corpus resolution, pruned shard
reading, the parallel trainer, checkpoints, events, and the dashboard.
`tests/` covers it against local parquet fixtures; the suite runs
offline.

```sh
uv run pytest
```
