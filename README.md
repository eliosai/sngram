# sngram

Sparse n-gram extraction for regular-expression search indexing.

A trigram index stores every overlapping 3-byte window. Most of those
windows are noise. sngram weights every byte pair and keeps only the
substrings whose two border pairs outweigh everything between them. The
kept grams vary in length and land on the distinctive parts of the text.
At query time a regex folds into a boolean plan over gram presence, in
the spirit of Russ Cox's Google Code Search analysis but with sparse
covering in place of trigram extraction. The plan matches a superset of
what the regex matches. A prefilter built from it never misses a match,
and the real regex verifies the candidates it admits.

The weight table decides where gram borders land. Rare byte pairs score
high and common pairs score low, so selectivity comes from the training
data. The tables are trained on terabytes of blended source code and
multilingual web text.

The project ships four surfaces.

## The Rust crates

`sngram` is the core library and `sngram-types` holds the shared value
types. Trained weight tables are embedded in `sngram` behind one Cargo
feature per training-data tier.

```toml
[dependencies]
sngram = { version = "0.5", features = ["12tb"] }
sngram-types = "0.5"
```

```rust
use sngram::{query, scan};
use sngram_types::ScanEvent;
use std::io::Cursor;

let table = sngram::weights();
let doc = b"fn max_file_size() -> u64 { 0 }";

// index side: every gram arrives with its final 64-bit index key
scan(&table, Cursor::new(doc), |event| {
    if let ScanEvent::Gram(gram) = event {
        let _key = gram.key; // store this in your inverted index
    }
})?;

// query side: a regex becomes a boolean gram query
let plan = query(&table, r"max_\w+_size")?;
```

`scan` reads one `BufRead` stream, allocates nothing per gram, and ends
with a `ScanEvent::Finish` summary of document metadata mined in the
same pass. `query` returns a `QueryPlan` whose needles carry the same
keys `scan` emits. Concerns like fixed-string escaping, smart case, and
multi-pattern OR joining belong above `query`, encoded into the single
pattern you pass in.

Training from Rust lives behind the `learn` feature as
`sngram::learn::BigramCounter`. The README in [crates/lib](crates/lib)
covers the library in more depth.

## The Python package

`crates/python` is the standalone `sngram` package for Python, built
with maturin over the same Rust core. It has no runtime dependencies
and mirrors the Rust surface: scan, query planning, weight tables, and
the GIL-free training counters. It lands on PyPI with v1 once the final
tables are minted.

```python
import sngram

table = sngram.weights()
result = sngram.scan(table, b"fn main() {}")
result.grams                 # [(start, end, key), ...]
result.summary.byte_len      # scan-derived document metadata

plan = sngram.query(table, r"max_\w+_size")
plan.op, plan.grams          # boolean query over index keys
plan.needs[0].satisfied_by(result.summary)
```

[crates/python/README.md](crates/python/README.md) documents the full
surface, including plan tuning and a worked inverted-index example.

## The trainer

`train/` is the `sngram-train` project, the pipeline that mints weight
tables. It streams the Stack v2 and Software Heritage blend, counts
byte pairs through the Rust core at around 3 GB/s per core, checkpoints
continuously, and mints a table every terabyte.

```sh
cd train
uv sync
uv run sngram train --limit 1GB     # smoke run
uv run sngram train --mint-dir ./bins
uv run sngram inspect bins/final_weights.bin
```

Credentials go in `train/.env`. [docs/training.md](docs/training.md)
specifies the one remaining production run and its acceptance gates.

## The eg CLI

`crates/eg` is a code search tool built on the index: a ripgrep-style
searcher that prefilters files through the sparse index and verifies
candidates with the real regex engine. Its `eg-indexd` daemon builds,
watches, and refreshes indexes in the background, so queries after the
first build hit a warm index.

```sh
just eg release
target/release/eg 'max_\w+_size' ~/src/linux
target/release/eg --bench 'max_\w+_size' ~/src/linux
```

On the Linux kernel tree the indexed path runs the benchmark suite 2.3x
faster than scanning, with an index at 0.90x the corpus size and zero
false negatives. [crates/eg/README.md](crates/eg/README.md) covers the
CLI, the daemon, and the benchmark modes.

## Docs

- [docs/architecture.md](docs/architecture.md) the system in one page
- [docs/index-format.md](docs/index-format.md) postings-v9 on disk
- [docs/query-planning.md](docs/query-planning.md) regex to plan to candidates
- [docs/daemon.md](docs/daemon.md) who builds and owns indexes
- [docs/benchmarking.md](docs/benchmarking.md) how to measure claims
- [docs/training.md](docs/training.md) the final training run

## License

[MIT](LICENSE)
