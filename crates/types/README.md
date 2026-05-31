# sngram-types

Core types for sparse n-gram weight tables, shared by `sngram` and
`sngram-weights`.

```toml
[dependencies]
sngram-types = "0.2"
```

- `WeightTable`: a 256x256 grid of byte-pair weights, loaded from a validated
  262,160-byte binary (magic, version, CRC32, then 65,536 `u32` weights). Look
  up a pair with `table.weight(c1, c2)`.
- `Content`: a zero-copy view over the bytes you index.
- `IndexGram` and `QueryGram` with their iterators: the grams produced when
  indexing a document or decomposing a query.

`sngram` re-exports all of these, so most code depends on `sngram` and never
names this crate.

## License

[MIT](https://github.com/eliosai/sngram/blob/main/LICENSE)
