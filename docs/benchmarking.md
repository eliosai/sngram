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
through `rg` when it is on PATH, with wall times and speedups.

```json
"comparison": {
  "indexed_wall_ms": 12.8,
  "scan_wall_ms": 104.5,
  "rg_wall_ms": 104.2,
  "speedup_scan": 8.19,
  "speedup_rg": 8.16
}
```

Set `EG_BENCH_NO_COMPARE=1` to skip the comparison runs; the suite sets
it on its children so nested runs never double-count.

## The suite

```sh
cd /path/to/corpus && target/release/eg --bench
```

Bare `--bench` runs the embedded 296-query TSV suite
(`crates/eg/src/index/data/fp-queries.tsv`), three legs per query:
indexed, `--no-index`, and `rg`. Per-query rows report wall times and
false positives; per-class aggregation groups by the id prefix before
`_`. The run fails if any query's indexed hits diverge from its scan
hits, so zero false negatives is enforced, not observed.

The summary line carries the headline numbers:

```
summary regexes=296 ... false_positive_pct=27.76 false_negative_rows=0
index_bytes=1424769397 corpus_bytes=1585056108 index_ratio=0.90
```

## Corpora and recipes

The Linux kernel checkout is the optimization corpus; a structurally
different mixed-language checkout guards against overfitting:

```sh
just suite ~/ripos/linux
just guard          # gitoxide
```

Rules that keep numbers honest: benches get a quiet machine with no
concurrent cargo builds, hot-path claims compare indexed `eg`,
`eg --no-index`, and `rg` on the same corpus and output mode, and
results are reported with their command lines.

## Library benches

Divan microbenches for the scan and query hot paths:

```sh
cargo bench -p sngram-benches --bench extract
cargo bench -p sngram-benches --bench query
cargo bench -p sngram-benches --bench counter
```
