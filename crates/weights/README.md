# sngram-weights

Pre-trained sparse n-gram weight tables for `sngram`, embedded at compile
time. The official 0.5 tables currently exposed are:

- `500gb`
- `1tb`
- `2tb`
- `3tb`
- `4tb`
- `5tb`
- `6tb`
- `7tb`
- `8tb`
- `9tb`
- `10tb`
- `11tb`
- `12tb`

The `100gb` bootstrap table is kept in `data/` for provenance, but it is not
exposed as a crate feature.

```toml
[dependencies]
sngram-weights = { version = "0.5", features = ["5tb"] }
```

```rust
let table = sngram_weights::weights();
let table_hash = table.fingerprint();
```

`WeightTable::fingerprint()` is a stable 64-bit identity value for index
manifests. It is not a cryptographic authenticity check; embedded table
payloads are validated by the build before the crate is compiled.

## License

[MIT](https://github.com/eliosai/sngram/blob/main/LICENSE)
