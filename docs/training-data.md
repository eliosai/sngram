# Training Data Contract

Decision date: 2026-07-23.

The production run trains on **5.11 TB of effective UTF-8 bytes**, the full
supply of the curated corpus under a 6 TB balanced target. Effective bytes
include the inverse weights applied to sampled small files; fetched bytes
measure content read from object storage.

## Sources

- Corpus: the `eliosai/sngram-train` dataset on the Hugging Face Hub,
  132,621,482 sampled objects as sharded jsonl plus a `manifest.json` sidecar
- Content: `s3://softwareheritage/content/{blob_id}`, public, read anonymously
- Metadata origin: `bigcode/the-stack-v2-dedup` at the revision recorded in
  the sidecar

The published dataset is the exact corpus: the balanced distribution below
is baked into its rows, so training streams the dataset and consumes every
row once, with no local index and no runtime allocation logic. The sidecar
names the corpus revision, and a checkpoint from a different revision fails
loudly instead of training on the wrong distribution.

## Areas

Measured supply hard-caps the two areas that matter most: clean deduplicated
source code tops out at 1.94 TB and prose at 0.72 TB. Only JSON, HTML, and
CSV can grow past their balanced share. The area weights below put code at
the largest share that still clears 5 TB of total corpus and size the
elastic formats to balance near 9 percent each.

| Area | Weight | Share |
| --- | ---: | ---: |
| Core programming | 2.28 TB | 38.0% |
| Config / build / infra | 1.18 TB | 19.7% |
| Docs / prose / markup | 0.84 TB | 14.0% |
| Web / UI / templates | 0.82 TB | 13.7% |
| Data / query / schema | 0.70 TB | 11.6% |
| Long-tail | 0.18 TB | 3.0% |

These shares were applied when the corpus was published: the target was
apportioned across areas with exact integer arithmetic, and inside each
area max-min fairness leveled every format up together, with exhausted
formats redistributing their missing bytes across the rest of the area.
The published rows are that allocation, so the trainer needs none of it.

## Row Admission

The corpus rejects vendor, generated, empty, and oversized rows. The size
ceiling is 2 MiB, or 4 MiB for docs. Formats prone to generated blobs carry
tighter per-file ceilings, from 160 KiB for YAML to 768 KiB for HTML.
Jupyter notebooks and hOCR dumps are excluded outright: base64 cells and OCR
layout markup teach a byte-pair table nothing about code.

Files below 16 KiB use deterministic inverse-probability sampling. For a
file of size `n`:

```text
w = next_power_of_two(ceil(16 KiB / n))
inclusion probability = 1 / w
effective counts = sampled counts * w
```

Files at least 16 KiB have weight one. Every eligible byte keeps the correct
expected contribution without hundreds of millions of small S3 reads.

## Training Flow

1. Stream corpus rows from the Hub dataset
2. Fetch each object concurrently from the content bucket, bounded reads
3. Decode to UTF-8 and count byte pairs through the Rust `BigramCounter`
4. Checkpoint the counter and the stream position every minute
5. Mint one final provenance-stamped table when the stream ends

Missing objects, invalid encodings, and empty decoded content are logged and
skipped; the measured loss rate is around one object in 200,000. A killed
run resumes from its last checkpoint and reproduces the identical table.

## Environment

- `HF_TOKEN`: read access to the corpus dataset, from the environment or
  `train/.env`; content needs no credentials

```sh
cd train
uv sync
uv run sngram train --mint-dir ./runs/r1
```
