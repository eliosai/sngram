"""Sparse n-gram extraction, regex query planning, and weight-table training.

The compiled core does the work; this package re-exports it. The corpus
training pipeline lives in the separate ``sngram-train`` project.

Index side::

    import sngram
    table = sngram.weights()                             # embedded production table
    keys = sngram.scan_hashes(table, b"fn main() {}")   # little-endian u64s
    # numpy view, zero-copy: np.frombuffer(keys, dtype=np.uint64)

Query side::

    plan = sngram.query(table, r"max_\\w+_size")
    plan.op, plan.grams, plan.children                   # boolean key query

Training::

    counter = sngram.BigramCounter()
    counter.count_arrow(arrow_table)                     # GIL-free, zero-copy
    open("weights.bin", "wb").write(counter.to_table_bytes())
"""

from sngram._core import (
    BigramCounter,
    QueryPlan,
    WeightTable,
    gram_hash,
    query,
    scan,
    scan_hashes,
    weights,
)

__version__ = "0.5.0"

__all__ = [
    "BigramCounter",
    "QueryPlan",
    "WeightTable",
    "__version__",
    "gram_hash",
    "query",
    "scan",
    "scan_hashes",
    "weights",
]
