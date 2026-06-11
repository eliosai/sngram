# sngram

Sparse n-gram extraction for regular-expression search indexing.

A trigram index extracts every overlapping 3-byte window, which produces a lot
of redundant, unselective tokens. Sparse n-grams cut that down: weight every
byte pair, then keep only the substrings whose two border pairs outweigh
everything inside them. The tokens vary in length and carry more signal. At
query time a regex folds into a `QueryPlan`, a boolean query over gram presence
faithful to Russ Cox's Google Code Search analysis but with sparse covering in
place of trigram extraction, so a regex turns into a handful of selective
lookups instead of dozens of common trigrams.

The weight function controls selectivity. Score rare byte pairs high and common
pairs low, and the grams land on the distinctive parts of the text. `sngram`
learns those weights from terabytes of blended real text (source code +
multilingual web), and `sngram-weights` ships the trained tables once minted.

## Install

```toml
[dependencies]
sngram = "0.5"
```

## Index and query

```rust
use sngram::{query, scan, Pattern};
use sngram_types::{Content, WeightTable};

let table = WeightTable::from_bytes(&std::fs::read("bins/5tb_weights.bin")?)?;
let doc = Content::new(b"fn max_file_size() -> u64 { 0 }");

// Every sparse gram arrives with its 64-bit index key, computed in O(1)
// from rolling prefix hashes — store it straight into your inverted index.
scan(table, &doc, |start, end, hash| {
    let _gram = &doc.as_bytes()[start..end];
    let _key = hash;
});

// Fold a regex into a boolean gram query to prefilter candidates.
// Gram::hash() on the plan's grams yields the same keys scan emitted.
let plan = query(table, &Pattern::new(r"max_\w+_size").unwrap());
```

`scan` allocates nothing; each emission carries the gram's span and its 64-bit
rolling hash, so building index keys costs no second pass over the bytes.
`query` folds a regex into a `QueryPlan` (`All`, `None`, or nested `And`/`Or`
over `Gram` bags; `Gram` stores up to 22 bytes inline and hashes to the same
keys) to intersect against the index. The plan matches a superset of what the
regex matches, so a prefilter built from it never misses a match the index
could find; the real regex verifies the candidates.

`StreamScanner` indexes content fed in chunks, holding only a bounded window
instead of the whole document, and emits exactly the grams and hashes `scan`
would over the concatenation. Enable the `stream` feature for
`StreamScanner::index_reader`, which drives it from any
`tokio::io::AsyncBufRead`, reusing the reader's buffer. Upgrading from 0.4:
the emit callbacks gained the hash argument, `index`/`IndexGram(s)` are gone,
and index keys changed — reindex.

## Weights

A table is a 256x256 grid: one `u32` per byte pair, 65,536 entries, plus a
16-byte header (magic, version, CRC32). 262,160 bytes, validated on load.

The 0.4-era tables are retired: the 0.5 trainer uses a blended corpus and a
new mint schedule (`100gb`, `500gb`, `1tb`, then every 5 TB to `50tb`).
`sngram-weights` will embed the new tables as they mint — until then every
size feature is a `compile_error!`, and you load a table you minted yourself:

```rust
let table = sngram_types::WeightTable::from_bytes(&std::fs::read("bins/5tb_weights.bin")?)?;
let w = table.weight(b'f', b'n');
```

## Minting your own

To train fresh weights from Rust, enable the `learn` feature for the bigram
counters and table serialization (the Python trainer below is the full
pipeline):

```toml
sngram = { version = "0.5", features = ["learn"] }
```

```rust
use sngram::learn::BigramCounter;

let counter = BigramCounter::new();
counter.process(b"fn main() {}");           // once per document
let bytes = counter.to_table_bytes();        // SPNG .bin, loads via WeightTable
```

Counting is per-value — no bigram straddles two documents — so the learned
table is a function of the data alone. The minted `.bin` files are what
`sngram-weights` will embed.

## Python

The `python/` uv project ships the `sngram` Python package: maturin-built
bindings (`scan`, `scan_hashes`, `query`, `gram_hash`, plus the
training counters with zero-copy, GIL-free Arrow ingestion) and the training
CLI.

```sh
cd python && uv sync
export HF_TOKEN=hf_...                   # or put it in .env
uv run sngram train --mint-dir ./bins    # 50 TB target, mints every 5 TB
uv run sngram train --limit 1GB          # smoke run
uv run sngram inspect bins/5tb_weights.bin
uv run sngram bench-ingest --workers 8   # offline pipeline benchmark
```

`train` streams the corpus mix (the-stack, finepdfs, fineweb-2,
starcoderdata, github-code) from Hugging Face with N parallel workers, blends
the datasets so every mint reflects the whole mix, counts through the Rust
core (~3 GB/s/core), mints exactly every 5 TB, checkpoints continuously, and
resumes exactly where it stopped. A live dashboard shows throughput, ETA to
the next mint, per-worker progress, and stalls; every event also lands in a
JSONL log next to the mints.

## License

[MIT](LICENSE)
