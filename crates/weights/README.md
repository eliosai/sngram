# sngram-weights

Pre-trained sparse n-gram weight tables for `sngram`, embedded at compile
time. The official 0.5 tables currently exposed are:

- `500gb`
- `1tb`
- `2tb`
- `3tb`
- `4tb`
- `5tb`

Larger bins are intentionally not exposed because they are not trusted for this
release line.

```toml
[dependencies]
sngram-weights = { version = "0.5", features = ["5tb"] }
```

```rust
let table = sngram_weights::weights()?;
```

For binaries that accept a table name at runtime, enable the tables you want to
ship and resolve them explicitly:

```rust
let spec = sngram_weights::get("5tb").expect("table feature is enabled");
let table_hash = spec.fingerprint();
let table = spec.load()?;
```

`BuiltinTable::fingerprint()` is a stable 64-bit identity value for index
manifests. It is not a cryptographic authenticity check; the embedded table
still validates its `SPNG` payload checksum when loaded.

## License

[MIT](https://github.com/eliosai/sngram/blob/main/LICENSE)
