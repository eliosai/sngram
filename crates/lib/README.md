# sngram

[![crates.io](https://img.shields.io/crates/v/sngram.svg)](https://crates.io/crates/sngram)
[![docs.rs](https://docs.rs/sngram/badge.svg)](https://docs.rs/sngram)
[![MIT](https://img.shields.io/crates/l/sngram.svg)](https://github.com/eliosai/sngram/blob/main/LICENSE)

Sparse n-gram extraction and regex query planning for code search.
Stateless, `Send + Sync`, one crate.

```sh
cargo add sngram
```

The crate embeds the trained production weight table. Turn the default
`weights` feature off to bring your own table through
`WeightTable::from_bytes`.

## How it works

A weight table assigns a `u32` to every byte pair. Rare pairs score high
and common pairs score low.

Indexing walks every byte pair with a monotonic stack and emits the
substrings whose two border weights beat all the internal weights. Those
sparse grams vary in length and carry more signal than fixed trigrams.
Every emission carries the gram's 64-bit rolling hash, computed in
constant time from prefix hashes maintained during the scan, so the
inverted-index key costs nothing extra.

Querying folds a regex into a `QueryPlan`, a boolean query over gram
presence and document facts. The regex is analyzed bottom-up: each literal
covers to the grams the scanner is guaranteed to emit for it, a maximal
cover for a lone literal and a minimal one per branch of a wide
alternation, and the byte counts, edges and lengths a match needs join
the plan as summary conditions. The plan matches a superset of what the
regex matches, so a candidate prefilter built from it never misses a
match. The real regex then verifies the candidates.

## API

Four functions, every type they name at the crate root.

```rust
use sngram::{is_binary, query, scan, PlanExpr};

fn main() -> Result<(), sngram::QueryError> {
    let table = sngram::weights();
    let doc = b"fn max_file_size() -> u64 { 0 }";

    // index side: every gram arrives with the key to store
    assert!(!is_binary(doc));
    let mut keys = Vec::new();
    let summary = scan(table, doc, |gram| keys.push(gram.key));
    assert_eq!(summary.gram_count as usize, keys.len());

    // query side: a regex becomes a boolean gram query
    let plan = query(table, r"max_\w+_size")?;
    assert!(matches!(plan.root(), PlanExpr::AllOf { .. }));
    Ok(())
}
```

`scan` takes one whole document and returns its `ScanSummary`, the
metadata mined in the same pass, while the callback receives each
`ScannedGram` with its key and content span. It applies no policy, so a
document with NUL bytes scans like any other.

`is_binary` is the policy: true when a NUL byte sits in the first 8 KiB.
That is the rule ripgrep's binary detection quits on, so an index that
skips what `is_binary` refuses skips exactly the files the verifier would
refuse, and a NUL past the window leaves the file indexed for the verifier
to judge. The index only ever adds candidates.

For valid patterns `query` is infallible: a pattern too broad to prefilter
yields `PlanExpr::All` and an impossible one yields `PlanExpr::None`.

```text
PlanExpr::All
PlanExpr::None
PlanExpr::AllOf { grams: Vec<GramNeedle>, needs: Vec<ScanNeed>, children: Vec<PlanExpr> }
PlanExpr::AnyOf { grams: Vec<GramNeedle>, needs: Vec<ScanNeed>, children: Vec<PlanExpr> }
```

`GramNeedle` stores finalized `GramKey` values, so query execution looks
up the same keys `scan` emitted. `ScanNeed` stores document-level
requirements that evaluate against the scan summary. The structure maps
onto an integer-array index directly: an `AllOf` gram bag is intersection
and an `AnyOf` gram bag is union. Once the index knows document
frequencies through `DfStats`, `QueryPlan::tune` reorders alternatives by
selectivity and drops bags too common to narrow anything.

`PlanExpr`, `GramNeedle` and `ScanNeed` are exhaustive on purpose. An
executor must handle every variant, so a new variant is a breaking change.

CLI concerns such as fixed-string escaping, smart case, multi-pattern OR
joining, and CRLF mode belong above `query`, encoded into the single
pattern you pass in.

## Features

| feature | adds |
|---|---|
| `weights` (default) | the embedded production weight table, `sngram::weights()` |
| `learn` | `sngram::learn`: `BigramCounter`, the byte-pair counter that trains fresh tables, and `LearnError` |

Count with `process` or `process_batch`, merge staging counters with
`merge`, and serialize with `to_table_bytes` in the format
`WeightTable::from_bytes` loads. Tables minted by the full pipeline carry
a provenance record naming the corpus revision and counted totals; read
it back with `table.provenance()`.

## Compatibility

0.8 folds `sngram-types` into this crate, takes `scan` over a byte slice,
returns the summary instead of a `ScanEvent::Finish` event, and moves the
binary rule into `is_binary`. Keys are unchanged, so an index built with
0.7 still answers 0.8 queries; only the set of files it holds may differ
under the new binary rule.

0.6 changed index keys to the emitted `GramKey`, so reindex when
upgrading from anything older: old index keys will not match new query
keys.

## License

[MIT](https://github.com/eliosai/sngram/blob/main/LICENSE)
