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
sngram-types = "0.5"
```

## Index and query

```rust
use sngram::{query, scan};
use sngram_types::{ScanEvent, WeightTable};
use std::io::Cursor;

let table = WeightTable::from_bytes(&std::fs::read("bins/5tb_weights.bin")?)?;
let doc = b"fn max_file_size() -> u64 { 0 }";

// Every sparse gram arrives with its 64-bit index key, computed in O(1)
// from rolling prefix hashes — store it straight into your inverted index.
scan(&table, Cursor::new(doc), |event| {
    if let ScanEvent::Gram(gram) = event {
        let _span = gram.span;
        let _key = gram.key; // store this in your inverted index
    }
})?;

// Fold a regex into a boolean gram query to prefilter candidates.
let plan = query(&table, r"max_\w+_size")?;
let _root = plan.root();
```

`scan` reads one `BufRead` stream, allocates nothing per gram, and emits
`ScanEvent::Gram` plus one final `ScanEvent::Finish` summary. Each gram carries
its content span and finalized `GramKey`; the summary carries document metadata
that was mined during the same pass.
`query` folds a regex into a `QueryPlan` rooted at `PlanExpr::All`,
`PlanExpr::None`, `PlanExpr::AllOf`, or `PlanExpr::AnyOf`. Its `GramNeedle`s are
the finalized keys to look up in the index, including folded alternatives when a
case-insensitive pattern needs them. The plan matches a superset of what the
regex matches, so a prefilter built from it never misses a match the index could
find; the real regex verifies the candidates. CLI
concerns such as fixed-string escaping, multiple-pattern OR joining, smart
case, and CRLF/byte regex mode should be encoded into the single regex pattern
before calling `query`.

Upgrading from 0.4: `scan` now takes a `BufRead` input and emits `ScanEvent`,
`query` now takes one regex pattern and returns `QueryPlan`, and index keys
changed — reindex.

## Weights

A table is a 256x256 grid: one `u32` per byte pair, 65,536 entries, plus a
16-byte header (magic, version, CRC32). 262,160 bytes, validated on load.

The 0.4-era tables are retired: the 0.5 trainer uses a capped blended corpus
and a new mint schedule (`100gb`, `500gb`, then every 1 TB to `15tb`).
`sngram-weights` embeds the released tables from `500gb` through `12tb`; the
`100gb` bootstrap table is kept as data but is not exposed as a crate feature.
You can also load a table you minted yourself:

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
export HF_TOKEN=hf_...                   # or put it in python/.env
uv run sngram train --mint-dir ./bins    # 15 TB target, mints every 1 TB
uv run sngram train --limit 1GB          # smoke run
uv run sngram inspect bins/5tb_weights.bin
```

`train` streams the capped corpus mix (CodeClippy, GitHub2025, the selected
high-star Stack v2 mirror, FinePDFs, FineWeb-2, StarCoderData config/markup,
and code/text blend sources) from Hugging Face with N parallel workers, blends
the datasets so every mint reflects the whole mix, counts through the Rust
core (~3 GB/s/core), mints every 1 TB, checkpoints continuously, and
resumes exactly where it stopped. A live dashboard shows throughput, ETA to
the next mint, per-worker progress, and stalls; every event also lands in a
JSONL log next to the mints.

## License

[MIT](LICENSE)
