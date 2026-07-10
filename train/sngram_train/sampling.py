"""Deterministic small-file sampling and inverse-weighted counting."""

from __future__ import annotations

import hashlib
import sys
from array import array
from collections import defaultdict
from collections.abc import Iterable
from dataclasses import dataclass

import sngram

SAMPLE_FLOOR = 16 * 1024


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
    groups: dict[int, list[bytes]] = defaultdict(list)
    effective = 0
    documents = 0
    for data, weight in rows:
        taken = min(len(data) * weight, limit - effective)
        if taken <= 0:
            break
        _group_segments(groups, data, weight, taken)
        effective += taken
        documents += 1
    counter = _count_groups(groups)
    counter.add_files(documents)
    return CountedBatch(counter, effective, documents)


def count_slices(rows: Iterable[WeightedSlice]) -> CountedBatch:
    """Count slices of conceptual inverse-weighted documents."""

    groups: dict[int, list[bytes]] = defaultdict(list)
    effective = 0
    documents = 0
    for row in rows:
        _group_slice(groups, row)
        effective += row.length
        documents += 1
    counter = _count_groups(groups)
    counter.add_files(documents)
    return CountedBatch(counter, effective, documents)


def _group_slice(groups: dict[int, list[bytes]], row: WeightedSlice) -> None:
    total = len(row.data) * row.weight
    if not row.data or row.offset < 0 or row.length <= 0 or row.offset + row.length > total:
        raise ValueError("weighted slice is outside its document")
    position = row.offset % len(row.data)
    remaining = row.length
    if position:
        taken = min(len(row.data) - position, remaining)
        groups[1].append(row.data[position : position + taken])
        remaining -= taken
    copies, prefix = divmod(remaining, len(row.data))
    if copies:
        groups[copies].append(row.data)
    if prefix:
        groups[1].append(row.data[:prefix])


def _group_segments(
    groups: dict[int, list[bytes]], data: bytes, weight: int, taken: int
) -> None:
    if not data or weight <= 0:
        raise ValueError("weighted documents must be non-empty with a positive weight")
    copies, prefix = divmod(taken, len(data))
    if copies:
        groups[copies].append(data)
    if prefix:
        groups[1].append(data[:prefix])


def _count_groups(groups: dict[int, list[bytes]]) -> sngram.BigramCounter:
    import pyarrow as pa

    total = sngram.BigramCounter()
    for factor, documents in groups.items():
        counter = sngram.BigramCounter()
        batch = pa.record_batch(
            {"content": pa.array(documents, type=pa.large_binary())}
        )
        counter.count_arrow(batch)
        total.merge(_scaled(counter, factor))
    return total


def _scaled(counter: sngram.BigramCounter, factor: int) -> sngram.BigramCounter:
    if factor == 1:
        return counter
    counts = array("Q")
    counts.frombytes(counter.snapshot())
    if sys.byteorder != "little":
        counts.byteswap()
    counts = array("Q", (count * factor for count in counts))
    if sys.byteorder != "little":
        counts.byteswap()
    scaled = sngram.BigramCounter()
    scaled.restore(
        counts.tobytes(),
        counter.pairs_processed * factor,
        counter.bytes_processed * factor,
        0,
    )
    return scaled
