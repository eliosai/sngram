# Training Data Contract

Decision date: 2026-07-24.

The production run trains on The Stack v3: decoded UTF-8 source text
streamed straight from the Hugging Face Hub. There is no external object
store and no local corpus; the parquet shards carry the file contents
inline.

## Source

- Dataset: `HuggingFaceCode/stack-v3-train`, license ODC-By 1.0, not gated
- 8196 snappy-compressed parquet shards under `data/`, 4.71 TB on the wire
- 15.9 TB of decoded source text, about 4.9 trillion tokens
- 713 languages, 172.9M rows, one row per repository
- GitHub snapshot 2025-08-07

Snappy expands about 3.3x measured on real shards, which is how 4.71 TB
on the wire becomes 15.9 TB of source text at the counter.

File content is embedded inline in `files[].content`, so a shard is
self-contained: no object store, no per-file fetch, no second request per
file. The reader selects `content`, `language`, `is_vendor`, and
`size_bytes` and leaves every other column undecoded.

The trainer pins the dataset revision at start and stamps it into the
checkpoint and the final provenance. A checkpoint from a different
revision or repo fails loudly instead of training on the wrong corpus.

## Distribution

Sampling is repository-realistic. One row is one repository, and the
trainer takes that repository's whole file mix as it stands. No
per-format targets, no reweighting, no sampling weights, no small-file
boost: elgrep indexes real source trees, so the training distribution is
the repository distribution. An earlier hand-balanced mixture measured
worse on four of five real corpora.

Vendored files count. `is_vendor` marks about 1.5 percent of files, and
people search vendored code like any other code, so dropping it would
train on a corpus nobody indexes. The counters track the vendored share
separately for reporting.

One filter applies: a file with null content is skipped. Nothing else is.

The realised language mix is recorded in every progress event, in the
mint event, and in the final table provenance, so a minted table can be
explained later.

## Training Flow

1. Resolve the dataset revision and shard listing from the Hub
2. Stream shards through parallel readers, row group by row group,
   with only the four file columns selected
3. Count byte pairs through the Rust `BigramCounter`, GIL released
4. Checkpoint the counter and every reader position each minute at a
   consistent quiesce
5. Mint one final provenance-stamped table when the stream ends

The checkpoint is taken with the readers held still, so the counter and
every reader position describe the same instant. Resume is byte-exact:
a killed run picks up at the byte it stopped on and reproduces the
identical table. Transient network failures retry in place with backoff
and resume from the current position; nothing is skipped.

## Measured

On the training machine, over real shards, 2026-07-25:

- about 340 MB/s of decoded source text at the counter
- about 100 to 115 MB/s on the wire, which is the link ceiling
- peak RSS about 2.2 GB
- about 13 hours for a full pass over all 8196 shards

## Environment

- `HF_TOKEN`: read access to the Hub, from the environment or `train/.env`
- `SNGRAM_DATASET_REPO`: overrides the dataset repo

```sh
cd train
uv sync
uv run sngram train --mint-dir ./runs/r1
```
