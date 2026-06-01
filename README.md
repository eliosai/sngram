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
learns those weights from terabytes of real source code, and `sngram-weights`
ships the trained tables.

## Install

```toml
[dependencies]
sngram = "0.3"
sngram-weights = "0.3"
```

## Index and query

```rust
use sngram::{query, scan};
use sngram::pattern::Pattern;
use sngram_types::Content;

let table = sngram_weights::weights();
let doc = Content::new(b"fn max_file_size() -> u64 { 0 }");

// Hash every sparse gram straight into your inverted index.
scan(table, &doc, |start, end| {
    let _gram = &doc.as_bytes()[start..end];
});

// Fold a regex into a boolean gram query to prefilter candidates.
let plan = query(table, &Pattern::new(r"max_\w+_size").unwrap());
```

`scan` allocates nothing and runs about 6x faster than `index` at 1 MB; reach
for it when you consume grams once. `index` returns a `Vec` when you need to
hold them. `query` folds a regex into a `QueryPlan` (`All`, `None`, or nested
`And`/`Or` over gram bags) to intersect against the index. The plan matches a
superset of what the regex matches, so a prefilter built from it never misses a
match the index could find; the real regex verifies the candidates.

## Weights

A table is a 256x256 grid: one `u32` per byte pair, 65,536 entries, plus a
16-byte header (magic, version, CRC32). 262,160 bytes, validated on load. Pick a
size with a `sngram-weights` Cargo feature; `weights()` returns the embedded
table.

```toml
sngram-weights = { version = "0.3", default-features = false, features = ["1tb"] }
```

```rust
let table = sngram_weights::weights();
let w = table.weight(b'f', b'n');
```

## Minting your own

Use the prebuilt tables from `sngram-weights` when you can. The `sngram` CLI
trains fresh weights, but it is not optimized: it streams datasets from Hugging
Face one file at a time, and a full 50 TB run can take north of 10 days
depending on your Hugging Face subscription and rate limits.

```sh
export HF_TOKEN=hf_...
sngram learn --mint-dir ./bins
sngram inspect ./bins/10tb_weights.bin
```

`learn` runs sequentially, resumes, and does not stop on rate limits. `inspect`
prints the commonest and rarest byte pairs. The minted `.bin` files are what
`sngram-weights` embeds.

## License

[MIT](LICENSE)
