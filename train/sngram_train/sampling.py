"""Deterministic small-file sampling and inverse-weighted counting."""

from __future__ import annotations

import hashlib
from collections.abc import Iterable
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import dataclass

import sngram

SAMPLE_FLOOR = 16 * 1024


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


def sample_weight(content_id: str, size: int) -> int | None:
    """Return a deterministic inverse sampling weight or skip the file."""

    if size <= 0:
        return None
    weight = _sample_weight(size)
    if weight == 1 or _hash(content_id) % weight == 0:
        return weight
    return None


def _sample_weight(size: int) -> int:
    ratio = (SAMPLE_FLOOR + size - 1) // size
    return 1 << (ratio - 1).bit_length()


def _hash(content_id: str) -> int:
    digest = hashlib.blake2b(
        content_id.encode(), digest_size=8, person=b"sngram-v3"
    ).digest()
    return int.from_bytes(digest, "little")


def count_weighted(rows: Iterable[tuple[bytes, int]], limit: int) -> CountedBatch:
    """Count weighted documents without crossing an effective-byte limit."""

    if limit < 0:
        raise ValueError("limit must be non-negative")
    documents: list[bytes] = []
    effective = 0
    counted = 0
    for data, weight in rows:
        taken = min(len(data) * weight, limit - effective)
        if taken <= 0:
            break
        _segment_rows(documents, data, weight, taken)
        effective += taken
        counted += 1
    counter = _count_documents(documents)
    counter.add_files(counted)
    return CountedBatch(counter, effective, counted)


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


def _segment_rows(
    documents: list[bytes], data: bytes, weight: int, taken: int
) -> None:
    if not data or weight <= 0:
        raise ValueError("weighted documents must be non-empty with a positive weight")
    copies, prefix = divmod(taken, len(data))
    documents.extend([data] * copies)
    if prefix:
        documents.append(data[:prefix])


def _count_documents(documents: list[bytes]) -> sngram.BigramCounter:
    import pyarrow as pa

    counter = sngram.BigramCounter()
    if documents:
        batch = pa.record_batch(
            {"content": pa.array(documents, type=pa.large_binary())}
        )
        counter.count_arrow(batch)
    return counter
