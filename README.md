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

The weight table is a byte-pair frequency table measured over terabytes
of real source repositories, whole trees with their natural mix of code,
config, markup, and docs, so rarity, and with it selectivity, comes from
the kind of text people search. The trained production table ships inside
the library.

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

The embedded 296-query suite runs every query twice, once through the
index and once through `eg --no-index`, on the same tree with the same
output mode. Measured 2026-07-25 on isolated corpus copies:

| Corpus | Index build | Suite vs scan | False positives | False negatives |
|---|---:|---:|---:|---:|
| linux (1.615 GB) | 13,101 ms | 7.87x | 26.56% | 0 |
| k8s | 2,611 ms | 7.59x | 39.64% | 0 |
| hass-core | 1,649 ms | 7.47x | 44.65% | 0 |
| django | 678 ms | 4.21x | 26.58% | 0 |

On linux the suite finishes in about 3,630 ms indexed against about
28,600 ms scanning, and the index is 1,467,104,135 bytes, 0.91x the
corpus. A false positive is a file the index hands to the verifier that
the regex then rejects, which costs time and never costs correctness.
The suite fails the run if any indexed hit set diverges from its scan hit
set, so zero false negatives is enforced rather than observed.

These are elgrep against itself. Comparisons with ripgrep and grep need a
ripgrep binary on PATH; `--bench` adds those legs when it finds one.
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
the same pass. It runs at about 208 MiB/s on code. `query` returns a
`QueryPlan` whose needles carry the same keys `scan` emits; the worst
plan in the bench set builds in 4.4 ms. Training from Rust lives behind
the `learn` feature as `sngram::learn::BigramCounter`. The README in
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

`train/` mints weight tables from The Stack v3
(`HuggingFaceCode/stack-v3-train`, ODC-By 1.0, ungated): 15.9 TB of
source text across 713 languages and 173M repositories, about 4.9
trillion tokens, from the GitHub snapshot of 2025-08-07. It streams the
8196 parquet shards straight from the Hugging Face Hub, reads file
content inline from `files[].content`, counts byte pairs through the
Rust core, checkpoints every minute, and mints one provenance-stamped
table when the stream ends. One row is one repository and the trainer
takes its whole file mix, so the trained distribution matches the source
trees people search. A full pass takes about 13 hours.

```sh
cd train
uv sync
uv run sngram train --shards 10     # smoke run
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
