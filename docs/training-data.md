# Training Data Contract

Decision date: 2026-07-24.

The production run trains on The Stack v3: decoded UTF-8 source text
streamed straight from the Hugging Face Hub. There is no external object
store and no local corpus; the parquet shards carry the file contents
inline.

## Source

- Dataset: `HuggingFaceCode/stack-v3-train`, license ODC-By 1.0, not gated
- 8196 parquet shards under `data/`, 4.71 TB on the wire, 15.9 TB decoded
- 172.9M rows, one row per repository, GitHub snapshot 2025-08-07
- Per file: `content`, `language`, `is_vendor` are the only fields read

The trainer pins the dataset revision at start and stamps it into the
checkpoint and the final provenance. A checkpoint from a different
revision or repo fails loudly instead of training on the wrong corpus.

## Distribution

The corpus is repository-grouped and the trainer takes each repository's
natural file mix. No per-format targets, no reweighting, no sampling
weights: elgrep indexes real source repositories, so the training
distribution is the repository distribution. A deliberately balanced v2
mixture measured worse on four of five real corpora, which is why v3
trains on the natural mix.

Two filters apply, both deterministic:

- `is_vendor` files are dropped; vendored code is duplicated noise
  (about 1.5 percent of files)
- files with null content are dropped

The realised language mix is recorded in every progress event, in the
mint event, and in the final table provenance, so a minted table can be
explained later.

## Training Flow

1. Resolve the dataset revision and shard listing from the Hub
2. Stream shards through parallel readers, row group by row group,
   with only the three file columns selected
3. Count byte pairs through the Rust `BigramCounter`, GIL released
4. Checkpoint the counter and every reader position each minute at a
   consistent pause
5. Mint one final provenance-stamped table when the stream ends

Transient network failures retry in place with backoff and resume from
the current position; nothing is skipped. A killed run resumes from its
last checkpoint and reproduces the identical table.

## Environment

- `HF_TOKEN`: read access to the Hub, from the environment or `train/.env`
- `SNGRAM_DATASET_REPO`: overrides the dataset repo

```sh
cd train
uv sync
uv run sngram train --mint-dir ./runs/r1
```
