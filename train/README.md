# sngram-train

The production Stack v2 trainer for sngram weight tables.

```sh
uv sync
uv run pytest
uv run sngram train --mint-dir ./runs/r1
```

The default run trains the full published corpus (clamped to its 5.11 TB
effective supply), checkpoints every minute, and mints one `final_weights.bin`
at the end. It resumes from the manifest and checkpoint in the mint dir.
Corpus rules and operating details live in
[`../docs/training-data.md`](../docs/training-data.md).

## corpus manifest

Training never scans Stack metadata. `sngram train` uses the local
`.manifest.sqlite3` in the mint dir, and when it is missing downloads the
published jsonl dataset from the Hugging Face assets repo
(`eliosai/sngram-train`, override with `SNGRAM_ASSETS_REPO`) and imports it
once. The corpus revision is pinned in `sngram_train/config.py`. Content
comes from the public Software Heritage bucket with bounded anonymous reads.

Reading the assets repo needs an `HF_TOKEN` in the environment or
`train/.env`; once the manifest is local no token is used.
