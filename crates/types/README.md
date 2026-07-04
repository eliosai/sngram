# sngram-types

Core types for sparse n-gram extraction and weight tables, shared by `sngram` and `sngram-weights`.

```toml
[dependencies]
sngram-types = "0.5"
```

- `WeightTable`: a 256x256 grid of byte-pair weights, loaded from and
  serialized to the validated `SPNG` binary format with `from_bytes` and
  `to_bytes`. Build synthetic or freshly learned tables with `from_weight_fn`,
  attach provenance with `with_provenance`, look up a pair with
  `table.weight(c1, c2)`, or grab the whole matrix with `table.matrix()` for
  hot loops.
- `Gram`: the canonical byte-string value used by query plans and index keys.
- `HashKey`: the shared hash vocabulary used by scanners and query-side grams,
  including direct byte hashing with `HashKey::hash_bytes`.
- `Content`: a zero-copy view over the bytes you index, with binary-content
  detection helpers.

These are the value types `sngram` builds its index and query API on. The main
`sngram` crate re-exports the public gram and hash types for compatibility.

## License

[MIT](https://github.com/eliosai/sngram/blob/main/LICENSE)
