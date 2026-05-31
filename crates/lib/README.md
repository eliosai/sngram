# sngram

Sparse n-gram extraction for regular-expression search indexing. Stateless,
`Send + Sync`.

```toml
[dependencies]
sngram = "0.2"
sngram-weights = "0.2"
```

## How it works

A weight table assigns a `u32` to every byte pair. Rare pairs score high, common
pairs score low.

Indexing walks every byte pair with a monotonic stack and emits the substrings
whose two border weights beat all the internal weights. Those sparse grams vary
in length and carry more signal than fixed trigrams. Hash them into an inverted
index.

Querying parses a regex, pulls fixed literals from both ends, and decomposes
each literal into the minimal covering set of sparse grams, a subset of what
indexing emits. Look those up. Every covering gram of a literal appears in the
index of any document that contains the literal, so a query never misses a match
the index could find.

## API

```rust
use sngram::{scan, index, query, Content, Pattern};

let table = sngram_weights::weights();
let doc = Content::new(b"fn max_file_size() -> u64 { 0 }");

scan(table, &doc, |start, end| {
    let _gram = &doc.as_bytes()[start..end];
});

let grams = index(table, &doc);

let pat = Pattern::new(r"max_\w+_size").unwrap();
let covering = query(table, &pat).unwrap();
```

| Function | Use it when |
|---|---|
| `scan` | You hash grams once into an index. About 6x faster than `index` at 1 MB. |
| `index` | You keep grams or iterate them more than once. |
| `query` | You have a regex and need the covering grams to look up. |

`query` returns `QueryError` when a pattern has no usable literals (`.*`,
`[a-z]+`) or its literals are too short to decompose.

`WeightTable`, `Content`, and the gram types are re-exported here, so you depend
only on `sngram`. Get a trained table from `sngram-weights`, or load your own
with `WeightTable::from_bytes`.

## License

[MIT](https://github.com/eliosai/sngram/blob/main/LICENSE)
