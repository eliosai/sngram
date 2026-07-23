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
data. The production table is trained on 5 TB of curated source code,
config, prose, and web text from Stack v2, and ships inside the library.

The project has four surfaces: a Rust crate, a Python package, a code
search CLI, and the trainer that mints weight tables.

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

`scan` reads one `BufRead` stream, allocates nothing per gram, and ends
with a `ScanEvent::Finish` summary of document metadata mined in the
same pass. `query` returns a `QueryPlan` whose needles carry the same
keys `scan` emits. Training from Rust lives behind the `learn` feature
as `sngram::learn::BigramCounter`. The README in
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

## The elgrep CLI

```sh
cargo install elgrep
```

`eg` is a code search tool built on the index: a ripgrep-style searcher
that prefilters files through the sparse index and verifies candidates
with the real regex engine. Its `eg-indexd` daemon builds, watches, and
refreshes indexes in the background, so every query after the first
build hits a warm index.

```sh
eg 'max_\w+_size' ~/src/linux
eg --no-index 'max_\w+_size' ~/src/linux   # plain scan for comparison
```

On the Linux kernel tree the indexed path answers common patterns 4x to
18x faster than ripgrep, with an index at 0.90x the corpus size and
zero false negatives. [crates/eg/README.md](crates/eg/README.md) covers
the CLI, the daemon, and the benchmark modes.

## The trainer

`train/` mints weight tables. It reads the published corpus manifest
from the Hugging Face Hub, streams content from the public Software
Heritage bucket, counts byte pairs through the Rust core, checkpoints
every minute, and mints one provenance-stamped table at the end.

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
