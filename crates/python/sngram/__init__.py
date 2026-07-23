"""Sparse n-gram extraction, regex query planning, and weight-table training.

The compiled core does the work; this package re-exports it. The corpus
training pipeline lives in the separate ``sngram-train`` project.

Index side::

    import sngram
    table = sngram.weights()                            # embedded production table
    result = sngram.scan(table, b"fn main() {}")        # grams + summary
    result.grams                                        # [(start, end, key), ...]
    result.summary.byte_len                             # scan-derived metadata
    keys = result.key_bytes()                           # little-endian u64s
    # numpy view, zero-copy: np.frombuffer(keys, dtype=np.uint64)

Query side::

    plan = sngram.query(table, r"max_\\w+_size")
    plan.op, plan.grams, plan.children                  # boolean key query
    plan.needs[0].satisfied_by(result.summary)          # metadata prefilter
    plan.tune(df, total_entries=n_docs, stop_df=n_docs // 2)

Training::

    counter = sngram.BigramCounter()
    counter.count_arrow(arrow_table)                    # GIL-free, zero-copy
    open("weights.bin", "wb").write(counter.to_table_bytes())
"""

from sngram._core import (
    BigramCounter,
    QueryPlan,
    ScanNeed,
    ScanResult,
    ScanSummary,
    WeightTable,
    query,
    scan,
)

try:
    from sngram._core import weights
except ImportError:  # built without the weights feature

    def weights() -> WeightTable:
        """The embedded production weight table."""
        raise RuntimeError("this build of sngram embeds no weight table")


__version__ = "0.5.0"

__all__ = [
    "BigramCounter",
    "QueryPlan",
    "ScanNeed",
    "ScanResult",
    "ScanSummary",
    "WeightTable",
    "__version__",
    "query",
    "scan",
    "weights",
]
