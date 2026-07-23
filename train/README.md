# sngram-train

The production Stack v2 trainer for sngram weight tables.

```sh
uv sync
uv run pytest
uv run sngram train --mint-dir ./bins
```

The default run trains 10 TB of effective bytes and resumes from the manifest
and checkpoint in `./bins`. Corpus rules and operating details live in
[`../docs/training-data.md`](../docs/training-data.md).

## corpus manifest

Training never scans Stack metadata. `sngram train` uses the local
`./bins/.manifest.sqlite3`, and when it is missing downloads the published one
from the Hugging Face assets repo (`eliosai/sngram-train`, override with
`SNGRAM_ASSETS_REPO`). The corpus revision is pinned in
`sngram_train/config.py`.

Re-minting the manifest is a deliberate, slow, one-time operation:

```sh
uv run sngram manifest build --publish
uv run sngram manifest publish
```

`build` scans the pinned Stack revision into a sampled manifest; `publish`
uploads the local manifest so every machine trains without scanning.
Publishing needs an `HF_TOKEN` with write access to the assets repo in
`train/.env`; training needs no token once the manifest is local or the
assets repo is readable.
