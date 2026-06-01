# sngram

Sparse n-gram extraction and regex query planning for code search. Stateless,
`Send + Sync`.

```toml
[dependencies]
sngram = "0.3"
sngram-weights = "0.3"
```

## How it works

A weight table assigns a `u32` to every byte pair. Rare pairs score high, common
pairs score low.

Indexing walks every byte pair with a monotonic stack and emits the substrings
whose two border weights beat all the internal weights. Those sparse grams vary
in length and carry more signal than fixed trigrams. Hash them into an inverted
index.

Querying folds a regex into a `QueryPlan`: a conservative boolean query over gram
presence. It is a faithful port of Russ Cox's Google Code Search analysis with
sparse covering in place of trigram extraction. The plan matches a superset of
what the regex matches, so a candidate prefilter built from it never misses a
match; the real regex then verifies the candidates.

## API

```rust
use sngram::{index, query, scan};
use sngram::pattern::Pattern;
use sngram::plan::QueryPlan;
use sngram_types::{Content, WeightTable};

let table = sngram_weights::weights();
let doc = Content::new(b"fn max_file_size() -> u64 { 0 }");

// index side: hash each gram into your inverted index
scan(table, &doc, |start, end| {
    let _gram = &doc.as_bytes()[start..end];
});
let grams = index(table, &doc);

// query side: a regex becomes a boolean gram query
let plan = query(table, &Pattern::new(r"max_\w+_size").unwrap());
```

`query` is infallible: a pattern too broad to prefilter yields `QueryPlan::All`
(scan everything, or reject it), an impossible one yields `QueryPlan::None`.

```rust
pub enum QueryPlan {
    All,
    None,
    And { grams: Vec<Vec<u8>>, sub: Vec<QueryPlan> }, // all grams present AND every sub
    Or  { grams: Vec<Vec<u8>>, sub: Vec<QueryPlan> }, // any gram present OR some sub
}
```

Gram leaves are raw bytes; hash them the same way you hashed the index. The
structure maps directly onto an integer-array index: with a postgres `int4[]`
column, an `And` bag is `grams @> ARRAY[..]` and an `Or` bag is
`grams && ARRAY[..]`.

| Function | Use it when |
|---|---|
| `scan` | You hash grams once into an index. About 6x faster than `index` at 1 MB. |
| `index` | You keep grams or iterate them more than once. |
| `query` | You have a regex and need its gram query plan. |

`Content`, `WeightTable`, and the gram types live in `sngram-types`. Get a
trained table from `sngram-weights`, or load your own with
`WeightTable::from_bytes`.

## License

[MIT](https://github.com/eliosai/sngram/blob/main/LICENSE)
