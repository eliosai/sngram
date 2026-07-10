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
