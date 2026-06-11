# sngram-weights

Pre-trained sparse n-gram weight tables for `sngram`, embedded at compile
time, one Cargo feature per training-volume tier.

**No tables are minted yet for 0.5.** The 0.4-era tables were retired with
the 0.5 training regime (blended code + multilingual web corpus, new mint
schedule). The new set is being trained: `100gb`, `500gb`, `1tb`, then every
5 TB up to `50tb`. Enabling any size feature before its table lands fails the
build with a clear message:

```text
error: sngram-weights: size `5tb` is not minted yet for 0.5. ...
```

Until the new tables ship, mint your own with the `sngram` Python trainer
(`sngram train`) and load it directly:

```rust
let table = sngram_types::WeightTable::from_bytes(&std::fs::read("bins/5tb_weights.bin")?)?;
```

## License

[MIT](https://github.com/eliosai/sngram/blob/main/LICENSE)
