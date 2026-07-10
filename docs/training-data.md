# Training Data Contract

Decision date: 2026-07-09.

The canonical run trains on **10 TB of effective UTF-8 bytes**. The available
Stack v2 roster has a 12 TB capacity envelope. Effective bytes include the
inverse weights applied to sampled small files; fetched bytes measure content
read from object storage.

## Sources

- Metadata: `bigcode/the-stack-v2-dedup` at one pinned revision
- Content: `s3://softwareheritage/content/{blob_id}`
- Encoding: each row's `src_encoding`, normalized to UTF-8 before counting

The trainer lists the pinned repository tree once. Each dataset config becomes
one format and gets scanned once. Unknown configs become separate long-tail
formats. `Text` rows split into docs, config, and data formats by path and
extension without rescanning the physical config.

## Areas

| Area | Capacity | Share |
| --- | ---: | ---: |
| Core programming | 5.20 TB | 43.33% |
| Docs / prose / markup | 2.30 TB | 19.17% |
| Config / build / infra | 1.50 TB | 12.50% |
| Web / UI / templates | 1.20 TB | 10.00% |
| Data / query / schema | 1.00 TB | 8.33% |
| Long-tail floor | 0.80 TB | 6.67% |

Every mint applies these shares to its cumulative effective-byte threshold with
exact integer apportionment. Format allocation inside each area uses max-min
fairness. All live formats advance at the same byte level. When a format
exhausts, the trainer redistributes its missing bytes equally among the remaining
formats in that area. One format cannot exceed 6% of its area's 12 TB capacity.

## Row Admission

The inventory rejects vendor, generated, empty, incomplete, and oversized rows.
The size ceiling is 2 MiB, or 4 MiB for docs. It has no minimum file size.

Files below 16 KiB use deterministic inverse-probability sampling. For a file
of size `n`, the trainer computes:

```text
w = next_power_of_two(ceil(16 KiB / n))
inclusion probability = 1 / w
effective counts = sampled counts * w
```

Files at least 16 KiB have weight one. This estimator gives every eligible byte
the correct expected contribution while avoiding hundreds of millions of small
S3 reads. A format containing only small files keeps its effective-byte target.
If its full eligible corpus is smaller than that target, normal exhaustion
redistribution applies.

## Inventory

The trainer writes `.manifest.sqlite3` under the mint directory. The manifest
stores sampled object references, weights, per-format capacity, the pinned
revision, and the roster hash. Buffered integer-key inserts keep its memory and
disk overhead bounded.

Each physical config commits in one SQLite transaction. An interrupted build
keeps completed configs and rolls back the current partial config. Resume starts
at the first incomplete config. A changed revision or format roster requires a
new mint directory.

## Counting

The canonical mint schedule is 100 GB, 500 GB, then every 1 TB through 10 TB.
For each threshold the trainer:

1. Computes exact cumulative area targets
2. Computes max-min cumulative format targets
3. Fetches at most one object per worker
4. Decodes and counts one bounded round through the Rust `BigramCounter`
5. Commits counts, format cursors, and progress as one durable cut
6. Mints only after every area and format reaches the barrier

No preview or in-flight counter enters a mint. A transient content failure aborts
the uncommitted round and resumes from the prior checkpoint. Missing objects,
invalid encodings, and empty decoded content are logged and skipped.

Gzip decompression stops at each row's declared length. With 64 workers and the
4 MiB document ceiling, fetched content occupies at most 256 MiB before the
bounded Arrow counting buffer. The trainer does not depend on allocator trimming
or an after-the-fact RSS soft limit.

## State And Telemetry

`.checkpoint.sqlite3` atomically stores the Rust count snapshot, effective and
fetched totals, format cursors, partial-object offsets, exhaustion state, mint
history, and the previous mint distribution for KL comparison.

Each `mint` event records its exact durable area and format composition. Summary
events separate fetched bytes from effective bytes. The dashboard shows both,
plus area targets, format deficits, RSS, rate, and KL from the previous mint.

## Environment

- `HF_TOKEN`: access to `bigcode/the-stack-v2-dedup`
- AWS credentials: optional when the Software Heritage bucket permits anonymous reads
- `SNG_SWH_ANONYMOUS=0`: force the boto credential chain

```sh
cd train
uv sync
uv run sngram train --mint-dir ./bins
```
