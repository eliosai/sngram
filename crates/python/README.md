# sngram

Sparse n-gram extraction and regex query planning for search indexing,
from Python.

A weight table scores every byte pair. Scanning keeps the substrings
whose two border pairs outweigh everything between them, so the kept
grams are few, variable in length, and selective. A regex folds into a
boolean plan over those grams. The plan matches a superset of what the
regex matches, which makes it a sound prefilter: it never misses a
match, and the real regex verifies what it admits.

The compiled core is the same Rust library that powers the `sngram`
crates. This package adds nothing on top except the bindings, has no
runtime dependencies, and releases GIL-free into the Rust core for scan
and training work.

## Install

```sh
pip install sngram
```

Wheels embed the trained production weight table. Building from source
needs a Rust toolchain and embeds the same table by default.

## Scanning

```python
import sngram

table = sngram.weights()
result = sngram.scan(table, b"fn max_file_size() -> u64 { 0 }")

result.grams               # [(content_start, content_end, key), ...]
result.summary.byte_len    # document metadata mined during the scan
result.summary.line_count
```

`result.key_bytes()` returns the keys alone as little-endian u64 bytes
for zero-copy handoff, for example
`np.frombuffer(result.key_bytes(), dtype=np.uint64)`.

Store each key in your inverted index. The key is final: sentinel and
case-folding details are already folded in, so query-side keys match
scan-side keys exactly. `scan` rejects binary input by raising, which
is the same gate the Rust scanner applies.

## Query planning

```python
plan = sngram.query(table, r"max_\w+_size")

plan.op          # "and" | "or" | "all" | "none"
plan.grams       # key alternatives per logical gram
plan.needs       # metadata conditions, testable against summaries
plan.children    # nested plans
```

A plan is a boolean tree. Under `"and"` every gram bag, need, and child
must hold; under `"or"` any one suffices. `"all"` means the index
cannot narrow the query and `"none"` means nothing can match. Every
gram bag is satisfied when any of its keys is present, and every need
evaluates against a scan summary with `need.satisfied_by(summary)`.

Once your index knows document frequencies, tune the plan:

```python
tuned = plan.tune(df, total_entries=n_docs, stop_df=n_docs // 2)
```

`df` is a callable from key to entry count. Tuning reorders gram
alternatives by selectivity and drops bags too common to narrow
anything, and the tuned plan stays sound.

## A complete prefilter

```python
class Index:
    def __init__(self, table, docs):
        self.postings, self.summaries = {}, []
        for ord_, doc in enumerate(docs):
            result = sngram.scan(table, doc)
            self.summaries.append(result.summary)
            for _, _, key in result.grams:
                self.postings.setdefault(key, set()).add(ord_)

    def admits(self, plan, ord_):
        if plan.op in ("all", "none"):
            return plan.op == "all"
        checks = (
            [any(ord_ in self.postings.get(k, ()) for k in alts) for alts in plan.grams]
            + [need.satisfied_by(self.summaries[ord_]) for need in plan.needs]
            + [self.admits(child, ord_) for child in plan.children]
        )
        return all(checks) if plan.op == "and" else any(checks)
```

Candidates that pass `admits` still need the real regex; everything the
regex would match is guaranteed to pass.

## Weight tables

```python
table = sngram.weights()                       # embedded production table
table = sngram.WeightTable.from_path("my.bin") # a table you minted
table.fingerprint                              # stable identity hash
table.provenance                               # who minted it, from what
table.to_bytes()                               # SPNG binary round trip
```

`WeightTable.from_weight_fn(fn)` builds synthetic tables for tests,
`with_provenance(record)` stamps a table before it ships, and
`table.matrix()` exposes all 65,536 weights as little-endian u32 bytes.

## Training counters

```python
counter = sngram.BigramCounter()
counter.count_arrow(arrow_table)   # zero-copy, GIL-free, per row
counter.process(b"one document")   # direct counting
data = counter.to_table_bytes()    # loads via WeightTable.from_bytes
```

`count_arrow` accepts anything exporting the Arrow PyCapsule interface
with a record-batch schema and counts all string and binary columns.
`snapshot()` and `restore()` checkpoint the counter exactly. The full
corpus training pipeline lives in the repository's `train/` project.

## License

[MIT](https://github.com/eliosai/sngram/blob/main/LICENSE)
