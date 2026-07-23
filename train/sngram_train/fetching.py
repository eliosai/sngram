"""Pipelined bounded content fetching with per-format carry."""

from __future__ import annotations

import threading
from collections.abc import Callable, Sequence
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import dataclass, replace
from queue import SimpleQueue
from typing import Protocol

from .manifest import Candidate
from .sampling import WeightedSlice


@dataclass(frozen=True)
class Fetched:
    candidate: Candidate
    data: bytes | None
    fetched_bytes: int
    error: str | None = None
    carried: bool = False


class ContentReader(Protocol):
    def read(self, blob_id: str, max_bytes: int) -> bytes: ...


def read_candidate(content: ContentReader, candidate: Candidate) -> Fetched:
    """Fetch one candidate and re-encode it as UTF-8."""

    try:
        raw = content.read(candidate.blob_id, candidate.length)
        data = _utf8(raw, candidate.encoding)
        if not data:
            raise ValueError("decoded content is empty")
        return Fetched(candidate, data, len(raw))
    except (FileNotFoundError, LookupError, UnicodeError, ValueError) as error:
        return Fetched(candidate, None, 0, str(error)[:300])


def _utf8(raw: bytes, encoding: str) -> bytes:
    text = raw.decode(encoding)
    if encoding.lower() in ("utf-8", "utf8", "ascii", "us-ascii"):
        return raw
    return text.encode("utf-8")


@dataclass(frozen=True)
class Consumption:
    slices: tuple[WeightedSlice, ...]
    cursor: int
    offset: int
    fetched_bytes: int
    objects: int
    errors: tuple[str, ...]


def bounded_items(
    items: tuple[Candidate, ...], remaining: int, offset: int
) -> tuple[Candidate, ...]:
    """Select a prefix whose estimated weighted bytes cover the remaining goal."""

    selected = []
    estimated = -offset
    for item in items:
        selected.append(item)
        estimated += item.length * item.weight
        if estimated >= remaining:
            break
    return tuple(selected)


def carry_estimate(rows: Sequence[Fetched], offset: int) -> int:
    """Weighted bytes still available inside carried rows."""

    estimate = 0
    for index, row in enumerate(rows):
        if row.data is None:
            continue
        available = len(row.data) * row.candidate.weight
        estimate += available - (offset if index == 0 else 0)
    return max(estimate, 0)


def consume(
    rows: list[Fetched], remaining: int, cursor: int, offset: int
) -> tuple[Consumption, list[Fetched]]:
    """Slice fetched rows up to the remaining goal and return the leftovers."""

    slices: list[WeightedSlice] = []
    errors: list[str] = []
    fetched = sum(row.fetched_bytes for row in rows if not row.carried)
    objects = 0
    for index, row in enumerate(rows):
        if row.data is None:
            cursor, offset = cursor + 1, 0
            errors.append(row.error or "content read failed")
            continue
        available = len(row.data) * row.candidate.weight - offset
        taken = min(available, remaining)
        if taken <= 0:
            return _done(slices, cursor, offset, fetched, objects, errors), rows[index:]
        slices.append(WeightedSlice(row.data, row.candidate.weight, offset, taken))
        remaining -= taken
        objects += 1
        if taken < available:
            partial = _done(slices, cursor, offset + taken, fetched, objects, errors)
            return partial, rows[index:]
        cursor, offset = cursor + 1, 0
    return _done(slices, cursor, offset, fetched, objects, errors), []


def _done(slices, cursor, offset, fetched, objects, errors) -> Consumption:
    return Consumption(tuple(slices), cursor, offset, fetched, objects, tuple(errors))


class FetchPool:
    """In-flight content batches keyed by format."""

    def __init__(
        self,
        pool: ThreadPoolExecutor,
        reader: Callable[[Candidate], Fetched],
        max_inflight: int,
    ) -> None:
        self._pool = pool
        self._reader = reader
        self._max_inflight = max_inflight
        self._batches: dict[str, list[Future]] = {}
        self._carry: dict[str, list[Fetched]] = {}
        self._lock = threading.Lock()
        self._left: dict[str, int] = {}
        self._complete: SimpleQueue[str] = SimpleQueue()

    def inflight(self) -> int:
        return sum(len(futures) for futures in self._batches.values())

    def saturated(self) -> bool:
        return self.inflight() >= self._max_inflight

    def headroom(self) -> int:
        return max(self._max_inflight - self.inflight(), 0)

    def has_batch(self, format_id: str) -> bool:
        return format_id in self._batches

    def carry(self, format_id: str) -> list[Fetched]:
        return self._carry.get(format_id, [])

    def submit(self, format_id: str, items: Sequence[Candidate]) -> None:
        futures = [self._pool.submit(self._reader, item) for item in items]
        self._batches[format_id] = futures
        if not futures:
            self._complete.put(format_id)
            return
        with self._lock:
            self._left[format_id] = len(futures)
        for future in futures:
            future.add_done_callback(lambda _f, key=format_id: self._one_done(key))

    def _one_done(self, format_id: str) -> None:
        with self._lock:
            left = self._left.get(format_id, 0) - 1
            if left > 0:
                self._left[format_id] = left
                return
            self._left.pop(format_id, None)
        self._complete.put(format_id)

    def wait_complete(self) -> str | None:
        """Block until some format's whole batch is fetched."""

        if not self._batches:
            return None
        return self._complete.get()

    def collect(self, format_id: str) -> list[Fetched]:
        futures = self._batches.pop(format_id)
        carried = self._carry.pop(format_id, [])
        return carried + [future.result() for future in futures]

    def store_carry(self, format_id: str, rows: Sequence[Fetched]) -> None:
        if rows:
            self._carry[format_id] = [replace(row, carried=True) for row in rows]
