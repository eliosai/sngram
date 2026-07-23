# sngram-train

The trainer that mints sngram weight tables.

It streams the published corpus row by row from the Hugging Face Hub,
fetches each object from the public Software Heritage bucket with
bounded anonymous reads, counts byte pairs through the Rust core with
the GIL released, checkpoints every minute, and mints one
provenance-stamped `final_weights.bin` when the stream ends. Nothing
is prefetched and nothing lands on disk except the checkpoint and the
final table. A killed run resumes from its checkpoint and reproduces
the identical table. A live dashboard shows throughput, per-group
progress, and skips; every event also lands in a JSONL log.

This project depends on the `sngram` library by path and is not
published. The library it drives lives in
[crates/python](../crates/python).

## Running

```sh
uv sync
uv run sngram train --limit 1GB      # smoke run
uv run sngram train --mint-dir ./runs/r1
uv run sngram inspect runs/r1/final_weights.bin
```

The published dataset is the exact training corpus, so a full run
consumes every row once: 132,621,482 objects, 3.73 TB of raw content,
5.11 TB effective after inverse small-file weights. The dataset repo
is `eliosai/sngram-train` (override with `SNGRAM_ASSETS_REPO`).
Reading it needs an `HF_TOKEN` in the environment or `train/.env`;
content needs no credentials.

The corpus contract lives in
[docs/training-data.md](../docs/training-data.md) and the production
run with its acceptance gates in
[docs/training.md](../docs/training.md).

## Layout

`sngram_train/` holds the pipeline: the corpus stream, bounded content
fetching, counting, checkpoints, events, and the dashboard. `tests/`
covers it against local fixtures; the suite runs offline.

```sh
uv run pytest
```
