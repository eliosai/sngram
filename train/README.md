# sngram-train

The trainer that mints sngram weight tables.

It trains on The Stack v3 (`HuggingFaceCode/stack-v3-train`, ODC-By 1.0,
ungated), reading everything from the Hugging Face Hub and nothing else.
File content rides inline in `files[].content`, so there is no object
store behind the dataset and no per-file fetch. Parallel fetchers spool
the parquet shards to disk while a bounded decoder reads only the file
columns, vendored files included, and counts byte pairs through the Rust
core with the GIL released. The run checkpoints every minute at a
consistent quiesce, resumes byte-exactly after a kill, retries transient
network failures, and mints one provenance-stamped `final_weights.bin`
when the stream ends. Nothing outlives the run on disk except the
checkpoint, the event log, and the final table; in-flight shards spool
under `.spool` in the mint directory and are deleted as they finish.

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

The corpus is 8196 snappy parquet shards, 4.71 TB on the wire, 15.9 TB
of decoded source text and about 4.9 trillion tokens across 713
languages and 172.9M repositories, from the GitHub snapshot of
2025-08-07. Snappy expands about 3.3x on real shards. One row is one
repository; the trainer takes each repository's natural file mix, so
the trained distribution matches real source trees. A full pass takes
about 13 hours at the measured 340 MB/s of decoded text, held back by a
100 to 115 MB/s wire ceiling, at a peak RSS of about 2.2 GB.

`--shards N` bounds a run to the first N shards and `--limit` caps
decoded bytes; both mint a table. `--workers` sets the reader count,
`--checkpoint-every` the checkpoint period, and `--no-resume` starts a
mint directory over. Reading the dataset needs an `HF_TOKEN` in the
environment or `train/.env`. Override the dataset with
`SNGRAM_DATASET_REPO`.

Interrupt or kill the run at any point; it resumes from the checkpoint
under the same mint directory and reproduces the identical table. The
live dashboard shows decoded throughput, progress with ETA, the
realised language mix, and errors; `--no-dashboard` turns it off and
every event still lands in a JSONL log.

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
