# sngram

Sparse n-gram extraction and regex query planning for code search. Stateless,
`Send + Sync`.

```toml
[dependencies]
sngram = "0.5"
```

## How it works

A weight table assigns a `u32` to every byte pair. Rare pairs score high, common
pairs score low.

Indexing walks every byte pair with a monotonic stack and emits the substrings
whose two border weights beat all the internal weights. Those sparse grams vary
in length and carry more signal than fixed trigrams. Every emission carries the
gram's 64-bit rolling hash, computed in O(1) from prefix hashes maintained
during the scan — your inverted-index key costs nothing extra.

Querying folds a regex into a `QueryPlan`: a conservative boolean query over gram
presence. It is a faithful port of Russ Cox's Google Code Search analysis with
sparse covering in place of trigram extraction. The plan matches a superset of
what the regex matches, so a candidate prefilter built from it never misses a
match; the real regex then verifies the candidates.

## API

```rust,no_run
use sngram::{query, scan, Pattern};
use sngram_types::{Content, WeightTable};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = std::fs::read("crates/weights/data/5tb_weights.bin")?;
    let table = WeightTable::from_bytes(&bytes)?;
    let doc = Content::new(b"fn max_file_size() -> u64 { 0 }");

    // index side: each gram arrives with its 64-bit index key
    scan(&table, &doc, |start, end, hash| {
        let _gram = &doc.as_bytes()[start..end];
        let _key = hash; // store this in your inverted index
    });

    // query side: a regex becomes a boolean gram query; Gram::hash() yields
    // the same key scan emitted for the same bytes
    let plan = query(&table, &Pattern::new(r"max_\w+_size")?);
    let _ = plan;
    Ok(())
}
```

`query` is infallible: a pattern too broad to prefilter yields `QueryPlan::All`
(scan everything, or reject it), an impossible one yields `QueryPlan::None`.

```rust,ignore
pub enum QueryPlan {
    All,
    None,
    And { grams: Vec<Gram>, sub: Vec<QueryPlan> }, // all grams present AND every sub
    Or  { grams: Vec<Gram>, sub: Vec<QueryPlan> }, // any gram present OR some sub
}
```

`Gram` stores up to 22 bytes inline (no heap allocation for typical grams),
dereferences to `[u8]`, and `Gram::hash()` produces the same 64-bit key that
`scan` emits — index side and query side always agree. The structure maps
directly onto an integer-array index: with a postgres `int8[]` column, an
`And` bag is `grams @> ARRAY[..]` and an `Or` bag is `grams && ARRAY[..]`.

| Item | Use it when |
|---|---|
| `scan` | You have the whole document in memory. The fastest path. |
| `StreamScanner` | You index content from a reader without buffering it whole; same grams and hashes as `scan`, bounded memory. |
| `query` | You have a regex and need its gram query plan. |

### Streaming

`StreamScanner` extracts from a document fed in chunks, holding only a bounded
window, so you can index large content straight from a reader without buffering
it whole. It emits exactly the grams and hashes `scan` would over the
concatenation. Reuse one scanner across documents — `finish()` resets it.

```rust,no_run
use sngram::StreamScanner;
use sngram_types::WeightTable;

fn index(table: &WeightTable) {
    let mut scanner = StreamScanner::new(table);
    scanner.push(b"fn max_file", |_gram, _hash| { /* insert into your index */ });
    scanner.push(b"_size() {}", |_gram, _hash| { /* ... */ });
    scanner.finish();
}
```

Enable the `stream` feature for `StreamScanner::index_reader`, which drives the
scanner from any `tokio::io::AsyncBufRead`, reusing the reader's own buffer:

```toml
sngram = { version = "0.5", features = ["stream"] }
```

### Training

The `learn` feature (off by default) adds `sngram::learn::{BigramCounter,
LocalTally}` — the byte-pair counters that train fresh weight tables.
`BigramCounter::to_table_bytes()` serializes the learned table in the format
`WeightTable::from_bytes` loads.

`Content` and `WeightTable` live in `sngram-types`. Load a table you minted
with `WeightTable::from_bytes`; `sngram-weights` will embed the official 0.5
tables as the training run mints them.

## 0.5 migration

- `scan`'s callback gained a third argument: `emit(start, end, hash)`.
- `StreamScanner::push` callbacks are now `emit(gram, hash)`.
- `index`, `IndexGram`, and `IndexGrams` are gone — collect from `scan` if you
  need a `Vec`, and use the emitted `hash` instead of hashing gram bytes.
- `QueryPlan` grams are `Gram` (deref to `[u8]`) instead of `Vec<u8>`.
- Index keys changed: the rolling hash replaces whatever you hashed gram bytes
  with before. **Reindex when upgrading** — old index keys will not match new
  query keys.

## License

[MIT](https://github.com/eliosai/sngram/blob/main/LICENSE)
