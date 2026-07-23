"""Inverse-weighted document counting into the shared bigram counter."""

from __future__ import annotations

from collections.abc import Iterable
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import dataclass

import sngram


@dataclass(frozen=True)
class WeightedDoc:
    data: bytes
    weight: int


@dataclass(frozen=True)
class CountedBatch:
    counter: sngram.BigramCounter
    effective_bytes: int
    documents: int


class CountSink:
    """Asynchronous document counting into one shared counter."""

    def __init__(self, counter: sngram.BigramCounter) -> None:
        self.counter = counter
        self.pool: ThreadPoolExecutor | None = None
        self._pending: list[Future] = []

    def submit(self, docs: tuple[WeightedDoc, ...]) -> None:
        if self.pool is None:
            self._count(docs)
            return
        self._pending.append(self.pool.submit(self._count, docs))

    def _count(self, docs: tuple[WeightedDoc, ...]) -> None:
        self.counter.merge(count_documents(docs).counter)

    def drain(self) -> None:
        pending, self._pending = self._pending, []
        for future in pending:
            future.result()


def count_documents(docs: Iterable[WeightedDoc]) -> CountedBatch:
    """Count whole documents, each expanded by its inverse sampling weight."""

    expanded: list[bytes] = []
    effective = 0
    counted = 0
    for doc in docs:
        if not doc.data or doc.weight <= 0:
            raise ValueError("weighted documents must be non-empty with a positive weight")
        expanded.extend([doc.data] * doc.weight)
        effective += len(doc.data) * doc.weight
        counted += 1
    counter = _count_expanded(expanded)
    counter.add_files(counted)
    return CountedBatch(counter, effective, counted)


def _count_expanded(documents: list[bytes]) -> sngram.BigramCounter:
    import pyarrow as pa

    counter = sngram.BigramCounter()
    if documents:
        batch = pa.record_batch(
            {"content": pa.array(documents, type=pa.large_binary())}
        )
        counter.count_arrow(batch)
    return counter
