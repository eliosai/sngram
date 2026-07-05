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
            let _span = gram.span;
            let _key = gram.key; // store this in your inverted index
        }
    })?;

    // query side: a regex becomes a boolean gram query
    let plan = query(&table, r"max_\w+_size")?;
    let _root = plan.root();
    Ok(())
}
```

For valid patterns, a query too broad to prefilter yields `PlanExpr::All`
(scan everything, or reject it), and an impossible one yields `PlanExpr::None`.

```rust,ignore
pub struct QueryPlan {
    root: PlanExpr,
}

pub enum PlanExpr {
    All,
    None,
    AllOf { grams: Vec<GramNeedle>, needs: Vec<ScanNeed>, children: Vec<PlanExpr> },
    AnyOf { grams: Vec<GramNeedle>, needs: Vec<ScanNeed>, children: Vec<PlanExpr> },
}
```

`GramNeedle` stores finalized `GramKey` values, so query execution looks up the
same 64-bit keys that `scan` emitted. `ScanNeed` stores document-level metadata
requirements mined by the scanner. The structure maps directly onto an
integer-array index: an `AllOf` gram bag is containment/intersection and an
`AnyOf` gram bag is union.

| Item | Use it when |
|---|---|
| `scan` | You have one byte stream and need sparse grams, hash keys, and final scan metadata. |
| `query` | You have one regex pattern and need a planned gram prefilter. |

### Training

The `learn` feature (off by default) adds `sngram::learn::BigramCounter`, the
byte-pair counter that trains fresh weight tables. Use `process_batch` for a
retryable batch, merge completed staging counters with `merge`, and serialize
with `BigramCounter::to_table_bytes()` in the format `WeightTable::from_bytes`
loads.

`WeightTable` lives in `sngram_types`. Load a table you minted with
`WeightTable::from_bytes`; `sngram-weights` embeds the official 0.5 tables.

## 0.5 migration

- `scan` now takes a `BufRead` input and emits `ScanEvent::Gram` plus one
  `ScanEvent::Finish` per callback.
- `query` now takes one regex pattern and returns `QueryPlan`.
- Collect from `scan` if you need a `Vec`, and use the emitted `GramKey`
  instead of hashing gram bytes.
- `QueryPlan` grams are `Gram` (deref to `[u8]`) instead of `Vec<u8>`.
- Index keys changed: the rolling hash replaces whatever you hashed gram bytes
  with before. **Reindex when upgrading** — old index keys will not match new
  query keys.

## License

[MIT](https://github.com/eliosai/sngram/blob/main/LICENSE)
