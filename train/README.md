# sngram-train

The trainer that mints sngram weight tables.

It reads everything from the Hugging Face Hub and nothing else, and it
trains on one of two corpora.

`blend` is the production mix and the default: nine families over 38
sources, 15 TB of family ceilings across code, config and markup,
technical text, English prose from FinePDFs, and a twelve-language
UTF-8 coverage slice. A planner feeds the counter from whichever family
sits furthest below its target share of counted bytes, so a mint
reflects the intended mix rather than the raw dataset sizes, and every
family and source stops at its byte ceiling.

`stack-v3` is the single-dataset path (`HuggingFaceCode/stack-v3-train`,
ODC-By 1.0, ungated), where file content rides inline in
`files[].content` and one row is one repository.

Parallel fetchers spool shards to disk while a bounded decoder reads
only the text column each source declares, vendored files included, and
counts byte pairs through the Rust core with the GIL released. Sources
differ by file layout (nested stack shards, flat parquet columns,
gzipped JSON lines) and by text field (`content`, `text`, `Body`); the
reader picks the right one per shard. The run checkpoints every minute
at a consistent quiesce, resumes byte-exactly after a kill, retries
transient network failures, and mints one provenance-stamped
`final_weights.bin` when the stream ends. A checkpoint is bound to the
corpus name and its pinned revisions, so resuming into a different
corpus is refused. Nothing outlives the run on disk except the
checkpoint, the event log, and the final table; in-flight shards spool
under `.spool` in the mint directory and are deleted as they finish.

This project depends on the `sngram` library by path and is not
published. The library it drives lives in
[crates/python](../crates/python).

## Running

```sh
uv sync
uv run sngram train --shards 2                    # smoke run over the blend
uv run sngram train --mint-dir ./runs/r1          # the full production blend
uv run sngram train --corpus stack-v3             # the single-dataset path
uv run sngram inspect runs/r1/final_weights.bin
```

The blend resolves to 53,379 shards and 13.5 TB on the wire against 15
TB of family ceilings, so the code families taper before they fill and
the rest hold the mix. The stack-v3 corpus is 8196 snappy parquet
shards, 4.71 TB on the wire and 15.9 TB of decoded source text across
713 languages and 172.9M repositories, from the GitHub snapshot of
2025-08-07.

`--corpus` picks the corpus. `--shards N` bounds a run to the first N
shards of every source and `--limit` caps decoded bytes; both mint a
table. `--workers` sets the reader count, `--checkpoint-every` the
checkpoint period, and `--no-resume` starts a mint directory over.
Reading the data needs an `HF_TOKEN` in the environment or
`train/.env`. Override the stack-v3 dataset with `SNGRAM_DATASET_REPO`.

Interrupt or kill the run at any point; it resumes from the checkpoint
under the same mint directory and reproduces the identical table. The
live dashboard shows decoded throughput, progress with ETA, the
realised language mix, and errors; `--no-dashboard` turns it off and
every event still lands in a JSONL log.

The corpus contract lives in
[docs/training-data.md](../docs/training-data.md) and the production
run in [docs/training.md](../docs/training.md).

## Layout

`sngram_train/` holds the pipeline. `corpus.py` owns the shapes both
corpora share and the choice between them, `blend.py` the nine-family
roster, `stack.py` the single-dataset path, `hub.py` pinned listings and
shard reads, `planner.py` the dispatch order and its ceilings,
`reader.py` the per-layout decoders, and `pipeline.py` the parallel
trainer with its checkpoints, events, and progress. `tests/` covers it
against local fixtures; the suite runs offline.

```sh
uv run pytest
```
