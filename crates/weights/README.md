# sngram-weights

The pre-trained production sparse n-gram weight table for `sngram`,
embedded at compile time behind the `production` feature.

```toml
[dependencies]
sngram-weights = { version = "0.5", features = ["production"] }
```

```rust
let table = sngram_weights::weights();
let table_hash = table.fingerprint();
```

The table is minted untuned by the Python trainer (see
`docs/final-training-run.md` at the repository root). Historical tier
tables live in git history; they are re-mintable from training
checkpoints and are not shipped.
