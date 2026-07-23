# sngram-train

The trainer that mints sngram weight tables.

It reads the published corpus manifest from the Hugging Face Hub,
streams content from the public Software Heritage bucket with bounded
anonymous reads, counts byte pairs through the Rust core with the GIL
released, checkpoints every minute, and mints one provenance-stamped
`final_weights.bin` at the end. A killed run resumes from its
checkpoint and reproduces the identical table. A live dashboard shows
throughput, per-area balance, and the slowest formats; every event also
lands in a JSONL log next to the checkpoint.

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

The default run trains the full published corpus, clamped to its
5.11 TB effective supply. The first run downloads the jsonl dataset
from the assets repo (`eliosai/sngram-train`, override with
`SNGRAM_ASSETS_REPO`) and imports it once; after that the mint
directory is self-contained. The corpus revision is pinned in
`sngram_train/config.py`.

Reading the assets repo needs an `HF_TOKEN` in the environment or
`train/.env`. Content needs no credentials, and once the manifest is
local no token is used.

The corpus contract lives in
[docs/training-data.md](../docs/training-data.md) and the production
run with its acceptance gates in
[docs/training.md](../docs/training.md).

## Layout

`sngram_train/` holds the pipeline: the Hub dataset import, the
manifest, goal planning, bounded fetching, counting, checkpoints,
events, and the dashboard. `tests/` covers it against local fixtures;
the suite runs offline.

```sh
uv run pytest
```
