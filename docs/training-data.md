# Training Data Contract

Decision date: 2026-07-23.

The production run trains on **5.11 TB of effective UTF-8 bytes**, the full
supply of the curated corpus under a 6 TB balanced target. Effective bytes
include the inverse weights applied to sampled small files; fetched bytes
measure content read from object storage.

## Sources

- Manifest: the `eliosai/sngram-train` dataset on the Hugging Face Hub,
  153,213,295 sampled objects as sharded jsonl plus a `manifest.json` sidecar
- Content: `s3://softwareheritage/content/{blob_id}`, public, read anonymously
- Metadata origin: `bigcode/the-stack-v2-dedup` at the revision pinned in
  `sngram_train/config.py`

The published dataset is the corpus. Training never scans Stack metadata; it
imports the dataset once into a local SQLite manifest and verifies the roster
hash recorded in the sidecar against the catalog the code builds. A drifted
contract fails loudly instead of training on the wrong distribution.

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

The trainer apportions its target across areas by these weights with exact
integer arithmetic. Inside each area, max-min fairness levels every format
up together; no format exceeds its area's per-format share cap. When a
format exhausts, its missing bytes redistribute across the remaining formats
in the area.

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

1. Download and import the published manifest, once
2. Apportion the effective target across areas, then max-min across formats
3. Fetch candidates concurrently from the content bucket, bounded per format
4. Decode to UTF-8 and count byte pairs through the Rust `BigramCounter`
5. Checkpoint the counter and per-format cursors every minute
6. Mint one final provenance-stamped table when the target fills

Missing objects, invalid encodings, and empty decoded content are logged and
skipped; the measured loss rate is around one object in 200,000. A killed
run resumes from its last checkpoint and reproduces the identical table.

## Environment

- `HF_TOKEN`: read access to the manifest dataset, from the environment or
  `train/.env`; content needs no credentials

```sh
cd train
uv sync
uv run sngram train --mint-dir ./runs/r1
```
