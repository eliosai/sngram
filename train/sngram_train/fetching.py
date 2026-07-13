"""Pipelined bounded content fetching with per-format carry."""

from __future__ import annotations

from collections.abc import Callable, Sequence
from concurrent.futures import FIRST_COMPLETED, Future, ThreadPoolExecutor, wait
from dataclasses import dataclass, replace
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
        data = raw.decode(candidate.encoding).encode("utf-8")
        if not data:
            raise ValueError("decoded content is empty")
        return Fetched(candidate, data, len(raw))
    except (FileNotFoundError, LookupError, UnicodeError, ValueError) as error:
        return Fetched(candidate, None, 0, str(error)[:300])


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
        self._batches[format_id] = [
            self._pool.submit(self._reader, item) for item in items
        ]

    def wait_complete(self) -> str | None:
        """Block until some format's whole batch is fetched."""

        if not self._batches:
            return None
        while True:
            for format_id in sorted(self._batches):
                if all(future.done() for future in self._batches[format_id]):
                    return format_id
            pending = [
                future
                for futures in self._batches.values()
                for future in futures
                if not future.done()
            ]
            wait(pending, return_when=FIRST_COMPLETED)

    def collect(self, format_id: str) -> list[Fetched]:
        futures = self._batches.pop(format_id)
        carried = self._carry.pop(format_id, [])
        return carried + [future.result() for future in futures]

    def store_carry(self, format_id: str, rows: Sequence[Fetched]) -> None:
        if rows:
            self._carry[format_id] = [replace(row, carried=True) for row in rows]
