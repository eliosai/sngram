# sngram-weights

Pre-trained sparse n-gram weight tables for `sngram`, baked into your binary at
compile time. The crate ships no data files and parses nothing on the hot path.

Each table is a 256x256 grid of byte-pair weights (rare pairs high, common pairs
low) learned by streaming open-source code. The size is how much code trained
it, and more code makes sharper weights.

```toml
[dependencies]
sngram-weights = "0.2"
```

```rust
let table = sngram_weights::weights();
let w = table.weight(b'f', b'n');
```

## Sizes are features

One Cargo feature per size selects the table. `weights()` returns whichever one
you enabled.

```toml
sngram-weights = { version = "0.2", default-features = false, features = ["1gb"] }
```

Minted sizes: `1gb`, `10gb`, `50gb`, `100gb`, `1tb`, `5tb`, `10tb`. The default
feature is `10tb`.

## Unminted sizes fail the build

Bigger sizes exist as features but are not trained yet. Enabling one is a
compile error, not a silent "unknown feature":

```text
error: sngram-weights: size `15tb` is not minted yet. Enable a minted size:
       1gb, 10gb, 50gb, 100gb, 1tb, 5tb, 10tb.
```

They turn into real tables as the learner mints them.

## Cost

`include_bytes!` embeds each table. The first call validates and decodes it into
a `&'static WeightTable` behind a `OnceLock`. Every later call is an atomic
load, with no allocation, CRC, or I/O.

## License

[MIT](https://github.com/eliosai/sngram/blob/main/LICENSE)
