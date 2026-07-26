# Benchmarking

Two modes, both built into `eg --bench`. Build release first; the repo
defaults to `target-cpu=native` with fat LTO.

## Single query

```sh
target/release/eg --bench PATTERN PATH
```

Emits one JSON report for the indexed run: stage timings (plan, catalog
probe, index open, tune, execute, verify), candidate and match counts,
false-positive stats, and index byte sizes. The report ends with a
`comparison` block: the same query is re-run through `--no-index` and
through `rg` when a ripgrep binary is on PATH, with wall times and
speedups. Without one, `rg_wall_ms` and `speedup_rg` come back null.

The block's shape, with placeholder values:

```json
"comparison": {
  "indexed_wall_ms": 12.8,
  "scan_wall_ms": 104.5,
  "rg_wall_ms": null,
  "speedup_scan": 8.16,
  "speedup_rg": null
}
```

Set `EG_BENCH_NO_COMPARE=1` to skip the comparison runs; the suite sets
it on its children so nested runs never double-count.

## The suite

```sh
cd /path/to/corpus && target/release/eg --bench
```

Bare `--bench` runs the embedded 296-query TSV suite
(`crates/eg/src/index/data/fp-queries.tsv`), two legs per query, indexed
and `--no-index`, plus a third `rg` leg when a ripgrep binary is on PATH.
Per-query rows report wall times and false positives; per-class
aggregation groups by the id prefix before `_`. The run fails if any
query's indexed hits diverge from its scan hits, so zero false negatives
is enforced, not observed.

The summary line carries the headline numbers. From the linux corpus on
2026-07-26:

```
summary regexes=296 ... false_positive_pct=26.56 false_negative_rows=0
index_bytes=1467104135 ... index_ratio=0.91
```

## Corpora and recipes

The Linux kernel checkout is the optimization corpus; structurally
different checkouts guard against overfitting. `just suite` takes the
corpus directory:

```sh
just suite ~/repos/linux
just suite ~/repos/k8s
just suite ~/repos/hass-core
just suite ~/repos/django
```

The four corpora as measured on 2026-07-26, against ripgrep 15.2.0 and
GNU grep 3.11:

| Corpus | Index build | Suite vs scan | Suite vs rg | False positives | False negatives |
|---|---:|---:|---:|---:|---:|
| linux (1.615 GB) | 11,880 ms | 8.45x | 8.29x | 26.56% | 0 |
| k8s (272 MB) | 2,659 ms | 8.31x | 7.98x | 39.64% | 0 |
| hass-core (179 MB) | 1,697 ms | 8.30x | 8.05x | 44.65% | 0 |
| django (38 MB) | 765 ms | 4.34x | 4.19x | 26.58% | 0 |

Rules that keep numbers honest: benches get a quiet machine with no
concurrent cargo builds, hot-path claims compare indexed `eg` against
`eg --no-index` on the same corpus and output mode, `rg` joins the
comparison only when a real ripgrep binary is on PATH, and results are
reported with their command lines.

## Library benches

Criterion microbenches for the scan and query hot paths, from the
`sngram-benches` crate at `crates/lib/benches`:

```sh
cargo bench -p sngram-benches --bench extract
cargo bench -p sngram-benches --bench query
cargo bench -p sngram-benches --bench counter
```

Pass `-- --test` to run each case once and check it still works without
paying for a full measurement. `scan` measures about 208 MiB/s on code
and the worst plan in the query set builds in 4.4 ms.

The `eg` end-to-end bench is Divan, not Criterion:

```sh
just eg bench       # cargo bench -p elgrep --bench index
```
