# sngram

Sparse n-gram extraction for regular-expression search indexing, and
elgrep, an indexed ripgrep alternative built on it.

## Sparse n-grams

A classic regex index cuts every document into trigrams: each
overlapping three-byte window becomes a key in an inverted index, a
query regex decomposes into the trigrams a match must contain, and
intersecting their posting lists yields the candidate documents.
Trigrams are a compromise. Two-byte windows make posting lists too
large to intersect quickly, four-byte windows make the key space too
large to store, and every trigram repeats two bytes of its neighbor,
so most of the index is redundancy.

Sparse n-grams replace the fixed window with a weighted one. A weight
table assigns a weight to every byte pair, high for rare pairs and low
for common ones. The scanner extracts every substring whose two border
pairs weigh strictly more than every pair between them. The extracted
grams vary in length and land on the distinctive parts of the text.

Because the weights are deterministic, the index side and the query
side stay in agreement. Indexing extracts every sparse gram a document
contains. Querying extracts far fewer: a covering algorithm derives a
minimal set of grams a match must contain, so a regex folds into a
small boolean plan with fewer posting lookups and fewer candidates
than trigram decomposition. The plan matches a superset of what the
regex matches. A prefilter built from it never misses a match, and the
real regex verifies the candidates it admits.

The weight table is a byte-pair frequency table measured over
terabytes of curated source code, config, prose, and web text, so
rarity, and with it selectivity, comes from real data. The trained
production table ships inside the library.

## elgrep

```sh
cargo install elgrep
```

`eg` carries ripgrep's search path and adds the sparse index in front:
the index narrows each query to candidate files, the regex engine
verifies them, and results match a plain scan exactly. The `eg-indexd`
daemon builds, watches, and refreshes indexes in the background, so
every query after the first build hits a warm index.

```sh
eg 'max_\w+_size' ~/src/linux
eg --no-index 'max_\w+_size' ~/src/linux   # plain scan for comparison
```

On the Linux kernel tree, with a hot daemon-owned index,
files-with-matches output, and identical hit sets (p50 of 9 runs):

| Pattern | Matched files | elgrep | ripgrep | grep | vs ripgrep |
|---|---:|---:|---:|---:|---:|
| `linus tor` | 0 | 10.2 ms | 185.9 ms | 1345.8 ms | 18.2x |
| `EXPORT_SYMBOL_GPL` | 3610 | 45.4 ms | 202.6 ms | 1093.1 ms | 4.5x |
| `copy_from_user` | 1224 | 19.2 ms | 199.3 ms | 1121.3 ms | 10.4x |
| `schedule_timeout` | 418 | 13.6 ms | 177.4 ms | 963.2 ms | 13.0x |

The index is 0.90x the corpus size, and the embedded 296-query suite
enforces zero false negatives on every run.
[crates/eg/README.md](crates/eg/README.md) covers the CLI, the daemon,
and the benchmark modes.

## The Rust crate

```sh
cargo add sngram --features weights
```

The `weights` feature embeds the trained production table. Everything
the API needs is exported from the one crate.

```rust
use sngram::{query, scan, ScanEvent};
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

`scan` reads one `BufRead` stream, allocates nothing per gram, and
ends with a `ScanEvent::Finish` summary of document metadata mined in
the same pass. `query` returns a `QueryPlan` whose needles carry the
same keys `scan` emits. Training from Rust lives behind the `learn`
feature as `sngram::learn::BigramCounter`. The README in
[crates/lib](crates/lib) covers the library in depth.

## The Python package

```sh
pip install sngram
```

The same Rust core, built with maturin. No runtime dependencies, and
scan and training work release the GIL.

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

`train/` mints weight tables. It streams the published corpus row by
row from the Hugging Face Hub, fetches each object from the public
Software Heritage bucket, counts byte pairs through the Rust core,
checkpoints every minute, and mints one provenance-stamped table when
the stream ends. Nothing is prefetched.

```sh
cd train
uv sync
uv run sngram train --limit 1GB     # smoke run
uv run sngram train --mint-dir ./runs/r1
uv run sngram inspect runs/r1/final_weights.bin
```

[docs/training.md](docs/training.md) specifies the production run and
its acceptance gates.

## Documentation

- [docs/architecture.md](docs/architecture.md) the system in one page
- [docs/index-format.md](docs/index-format.md) postings-v9 on disk
- [docs/query-planning.md](docs/query-planning.md) regex to plan to candidates
- [docs/daemon.md](docs/daemon.md) who builds and owns indexes
- [docs/benchmarking.md](docs/benchmarking.md) how to measure claims
- [docs/training.md](docs/training.md) the production training run
- [docs/training-data.md](docs/training-data.md) the corpus contract

## License

[MIT](LICENSE)
