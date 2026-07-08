# sngram-train

The training pipeline that mints sngram weight tables.

The trainer streams the Stack v2 and Software Heritage blend defined in
[docs/training-data.md](../docs/training-data.md), counts byte pairs
through the Rust core with the GIL released, checkpoints continuously,
and mints a table every terabyte. A crashed or interrupted run resumes
from its checkpoint. A live dashboard shows throughput, per-worker
progress, and the countdown to the next mint, and every event also
lands in a JSONL log next to the mints.

This project depends on the `sngram` library by path and is not
published. The library it drives lives in
[crates/python](../crates/python).

## Running

```sh
uv sync
uv run sngram train --limit 1GB      # smoke run
uv run sngram train --mint-dir ./bins
uv run sngram inspect bins/final_weights.bin
```

Credentials go in `.env` here: a Hugging Face token for Stack v2 and S3
keys for Software Heritage. [docs/training.md](../docs/training.md)
specifies the production run and the acceptance gates a minted table
must pass before it ships.

## Layout

`sngram_train/` holds the pipeline: dataset planning, streaming
workers, the counter and checkpoint plumbing, minting, metrics, and the
dashboard. `tests/` covers the pipeline against local fixtures; the
suite runs offline.

```sh
uv run pytest
```
