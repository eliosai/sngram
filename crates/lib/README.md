# sngram

Sparse n-gram extraction and regex query planning for code search. Stateless,
`Send + Sync`.

```toml
[dependencies]
sngram = "0.5"
sngram-types = "0.5"
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
use sngram::{query, scan};
use sngram_types::{ScanEvent, WeightTable};
use std::io::Cursor;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = std::fs::read("crates/weights/data/5tb_weights.bin")?;
    let table = WeightTable::from_bytes(&bytes)?;
    let doc = b"fn max_file_size() -> u64 { 0 }";

    // index side: each gram arrives with its 64-bit index key
    scan(&table, Cursor::new(doc), |event| {
        if let ScanEvent::Gram(gram) = event {
            let _bytes = gram.bytes;
            let _key = gram.hash; // store this in your inverted index
        }
    })?;

    // query side: a regex becomes a boolean gram query
    let plan = query(&table, r"max_\w+_size")?;
    let _key = plan.hash_key(); // use with each plan gram's hash_keyed(...)
    Ok(())
}
```

For valid patterns, a query too broad to prefilter yields `QueryExpr::All`
(scan everything, or reject it), and an impossible one yields `QueryExpr::None`.

```rust,ignore
pub struct QueryPlan {
    expr: QueryExpr,
    space: GramSpace,
}

pub enum QueryExpr {
    All,
    None,
    And { grams: Vec<Gram>, sub: Vec<QueryExpr> }, // all grams present AND every sub
    Or  { grams: Vec<Gram>, sub: Vec<QueryExpr> }, // any gram present OR some sub
}
```

`Gram` stores up to 22 bytes inline (no heap allocation for typical grams) and
dereferences to `[u8]`. Hash plan grams with `plan.hash_key()` so folded-space
queries look up the same 64-bit keys that `scan` emitted. The structure maps
directly onto an integer-array index: with a postgres `int8[]` column, an `And`
bag is `grams @> ARRAY[..]` and an `Or` bag is `grams && ARRAY[..]`.

| Item | Use it when |
|---|---|
| `scan` | You have one byte stream and need sparse grams, hash keys, and final scan metadata. |
| `query` | You have one regex pattern and need a planned gram prefilter. |

### Training

The `learn` feature (off by default) adds `sngram::learn::{BigramCounter,
LocalTally}` — the byte-pair counters that train fresh weight tables.
`BigramCounter::to_table_bytes()` serializes the learned table in the format
`WeightTable::from_bytes` loads.

`WeightTable` lives in `sngram_types`. Load a table you minted with
`WeightTable::from_bytes`; `sngram-weights` embeds the official 0.5 tables.

## 0.5 migration

- `scan` now takes a `BufRead` input and emits `ScanEvent::Gram` plus one
  `ScanEvent::Finish` per callback.
- `query` now takes one regex pattern and returns `QueryPlan`.
- `index`, `IndexGram`, and `IndexGrams` are gone — collect from `scan` if you
  need a `Vec`, and use the emitted `hash` instead of hashing gram bytes.
- `QueryPlan` grams are `Gram` (deref to `[u8]`) instead of `Vec<u8>`.
- Index keys changed: the rolling hash replaces whatever you hashed gram bytes
  with before. **Reindex when upgrading** — old index keys will not match new
  query keys.

## License

[MIT](https://github.com/eliosai/sngram/blob/main/LICENSE)
