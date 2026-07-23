"""Inverse-weighted slice counting into the shared bigram counter."""

from __future__ import annotations

from collections.abc import Iterable
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import dataclass

import sngram


class CountSink:
    """Asynchronous slice counting into one shared counter."""

    def __init__(self, counter: sngram.BigramCounter) -> None:
        self.counter = counter
        self.pool: ThreadPoolExecutor | None = None
        self._pending: list[Future] = []

    def submit(self, slices: tuple[WeightedSlice, ...]) -> None:
        if self.pool is None:
            self._count(slices)
            return
        self._pending.append(self.pool.submit(self._count, slices))

    def _count(self, slices: tuple[WeightedSlice, ...]) -> None:
        self.counter.merge(count_slices(slices).counter)

    def drain(self) -> None:
        pending, self._pending = self._pending, []
        for future in pending:
            future.result()


@dataclass(frozen=True)
class CountedBatch:
    counter: sngram.BigramCounter
    effective_bytes: int
    documents: int


@dataclass(frozen=True)
class WeightedSlice:
    data: bytes
    weight: int
    offset: int
    length: int


def count_slices(rows: Iterable[WeightedSlice]) -> CountedBatch:
    """Count slices of conceptual inverse-weighted documents."""

    documents: list[bytes] = []
    effective = 0
    counted = 0
    for row in rows:
        _slice_rows(documents, row)
        effective += row.length
        counted += 1
    counter = _count_documents(documents)
    counter.add_files(counted)
    return CountedBatch(counter, effective, counted)


def _slice_rows(documents: list[bytes], row: WeightedSlice) -> None:
    total = len(row.data) * row.weight
    if not row.data or row.offset < 0 or row.length <= 0 or row.offset + row.length > total:
        raise ValueError("weighted slice is outside its document")
    position = row.offset % len(row.data)
    remaining = row.length
    if position:
        taken = min(len(row.data) - position, remaining)
        documents.append(row.data[position : position + taken])
        remaining -= taken
    copies, prefix = divmod(remaining, len(row.data))
    documents.extend([row.data] * copies)
    if prefix:
        documents.append(row.data[:prefix])


def _count_documents(documents: list[bytes]) -> sngram.BigramCounter:
    import pyarrow as pa

    counter = sngram.BigramCounter()
    if documents:
        batch = pa.record_batch(
            {"content": pa.array(documents, type=pa.large_binary())}
        )
        counter.count_arrow(batch)
    return counter
