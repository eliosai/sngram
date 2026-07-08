# sngram-types

Core types for sparse n-gram extraction and weight tables, underneath
the `sngram` crate.

```toml
[dependencies]
sngram-types = "0.5"
```

- `WeightTable` is the 256x256 grid of byte-pair weights. It loads from
  and serializes to the validated `SPNG` binary format with `from_bytes`
  and `to_bytes`. Build synthetic or freshly learned tables with
  `from_weight_fn`, attach provenance with `with_provenance`, look up
  one pair with `weight(c1, c2)`, or take the whole matrix with
  `matrix()` for hot loops.
- `ScanEvent`, `ScannedGram`, and `ScanSummary` are what a scan emits:
  keyed grams and the final per-document metadata.
- `QueryPlan`, `PlanExpr`, `GramNeedle`, and `ScanNeed` describe the
  boolean candidate query a regex folds into, and `DfStats` is the
  trait a deployment implements to let `QueryPlan::tune` see document
  frequencies.
- `Gram`, `HashKey`, and `Content` are the byte-string, hashing, and
  binary-detection primitives underneath.

These are the value types the `sngram` crate builds its scan and query
API on.

## License

[MIT](https://github.com/eliosai/sngram/blob/main/LICENSE)
