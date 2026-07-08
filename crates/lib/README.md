# sngram

Sparse n-gram extraction and regex query planning for code search.
Stateless, `Send + Sync`.

```toml
[dependencies]
sngram = "0.5"
sngram-types = "0.5"
```

## How it works

A weight table assigns a `u32` to every byte pair. Rare pairs score
high and common pairs score low.

Indexing walks every byte pair with a monotonic stack and emits the
substrings whose two border weights beat all the internal weights.
Those sparse grams vary in length and carry more signal than fixed
trigrams. Every emission carries the gram's 64-bit rolling hash,
computed in constant time from prefix hashes maintained during the
scan, so the inverted-index key costs nothing extra.

Querying folds a regex into a `QueryPlan`, a conservative boolean query
over gram presence. It follows Russ Cox's Google Code Search analysis
with sparse covering in place of trigram extraction. The plan matches a
superset of what the regex matches, so a candidate prefilter built from
it never misses a match. The real regex then verifies the candidates.

## API

```rust,no_run
use sngram::{query, scan};
use sngram_types::ScanEvent;
use std::io::Cursor;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let table = sngram::weights();
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

`scan` reads one `BufRead` stream and finishes with a
`ScanEvent::Finish` summary of document metadata mined in the same
pass. For valid patterns `query` is infallible: a pattern too broad to
prefilter yields `PlanExpr::All` and an impossible one yields
`PlanExpr::None`.

```rust,ignore
pub enum PlanExpr {
    All,
    None,
    AllOf { grams: Vec<GramNeedle>, needs: Vec<ScanNeed>, children: Vec<PlanExpr> },
    AnyOf { grams: Vec<GramNeedle>, needs: Vec<ScanNeed>, children: Vec<PlanExpr> },
}
```

`GramNeedle` stores finalized `GramKey` values, so query execution
looks up the same keys `scan` emitted. `ScanNeed` stores document-level
requirements that evaluate against the scan summary. The structure maps
onto an integer-array index directly: an `AllOf` gram bag is
intersection and an `AnyOf` gram bag is union. Once the index knows
document frequencies, `QueryPlan::tune` reorders alternatives by
selectivity and drops bags too common to narrow anything.

CLI concerns such as fixed-string escaping, smart case, multi-pattern
OR joining, and CRLF mode belong above `query`, encoded into the single
pattern you pass in.

## Training

The `learn` feature, off by default, adds `sngram::learn::BigramCounter`,
the byte-pair counter that trains fresh weight tables. Count with
`process` or `process_batch`, merge staging counters with `merge`, and
serialize with `to_table_bytes` in the format `WeightTable::from_bytes`
loads.

`WeightTable` lives in `sngram-types`. This crate embeds trained tables
behind one feature per training-data tier (currently `12tb`). Enable
exactly one tier and load it with `sngram::weights()`.

## 0.5 migration

`scan` now takes a `BufRead` input and emits `ScanEvent::Gram` plus one
`ScanEvent::Finish` per stream. `query` takes one regex pattern and
returns `QueryPlan`. Index keys changed to the emitted `GramKey`, so
reindex when upgrading: old index keys will not match new query keys.

## License

[MIT](https://github.com/eliosai/sngram/blob/main/LICENSE)
