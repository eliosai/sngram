# sngram-types

Core types for sparse n-gram weight tables, shared by `sngram` and
`sngram-weights`.

```toml
[dependencies]
sngram-types = "0.4"
```

- `WeightTable`: a 256x256 grid of byte-pair weights, loaded from a validated
  262,160-byte binary (magic, version, CRC32, then 65,536 `u32` weights). Look
  up a pair with `table.weight(c1, c2)`.
- `Content`: a zero-copy view over the bytes you index.
- `IndexGram` and `IndexGrams`: the grams produced when indexing a document.

These are the value types `sngram` builds its index and query API on. Name this
crate directly to use them; `sngram` does not re-export.

## License

[MIT](https://github.com/eliosai/sngram/blob/main/LICENSE)
