"""Durable sampled object manifest with stable per-format cursors."""

from __future__ import annotations

import os
import sqlite3
import fcntl
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

from .errors import ConfigurationError


@dataclass(frozen=True)
class Candidate:
    format_id: str
    blob_id: str
    encoding: str
    length: int
    weight: int


@dataclass(frozen=True)
class ManifestBatch:
    items: tuple[Candidate, ...]
    cursor: int
    exhausted: bool


class ManifestBuilder:
    def __init__(self, path: Path, revision: str, roster_hash: str) -> None:
        self.path = path
        self.revision = revision
        self.roster_hash = roster_hash
        self._tmp = path.with_suffix(path.suffix + ".tmp")
        self._lock_path = path.with_suffix(path.suffix + ".lock")
        self._lock_handle = None
        self._connection: sqlite3.Connection | None = None
        self._sequence: dict[str, int] = defaultdict(int)
        self._capacity: dict[str, int] = defaultdict(int)
        self._exhausted: dict[str, bool] = {}
        self._keys: dict[str, int] = {}
        self._encodings: dict[str, int] = {}
        self._buffer: list[tuple[object, ...]] = []
        self._completed: set[str] = set()
        self._cursors: dict[str, tuple[int, int]] = {}

    def __enter__(self) -> ManifestBuilder:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self._lock_handle = self._lock_path.open("a+")
        try:
            fcntl.flock(self._lock_handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            self._lock_handle.close()
            self._lock_handle = None
            raise ConfigurationError("another process is building this manifest") from error
        new = not self._tmp.exists()
        try:
            self._connection = sqlite3.connect(self._tmp)
            if new:
                self._initialize()
            else:
                self._restore()
        except Exception:
            self._release_lock()
            raise
        return self

    def _initialize(self) -> None:
        assert self._connection is not None
        self._connection.executescript(_SCHEMA)
        self._connection.executemany(
            "INSERT INTO metadata VALUES (?, ?)",
            (("revision", self.revision), ("roster_hash", self.roster_hash)),
        )
        self._connection.commit()

    def _restore(self) -> None:
        assert self._connection is not None
        metadata = dict(self._connection.execute("SELECT key, value FROM metadata"))
        if metadata != {"revision": self.revision, "roster_hash": self.roster_hash}:
            raise ConfigurationError("partial manifest does not match this roster")
        for key, format_id, count, capacity, exhausted in self._connection.execute(
            "SELECT format_key, id, candidates, capacity, exhausted FROM formats"
        ):
            self._keys[format_id] = key
            self._sequence[format_id] = count
            self._capacity[format_id] = capacity
            self._exhausted[format_id] = bool(exhausted)
        for config, shard, row in self._connection.execute(
            "SELECT config, shard, row FROM completed_configs"
        ):
            self._completed.add(config)
            self._cursors[config] = (shard, row)
        self._encodings.update(
            (name, key)
            for key, name in self._connection.execute("SELECT key, name FROM encodings")
        )

    def add(self, candidate: Candidate) -> None:
        if self._connection is None:
            raise RuntimeError("manifest builder is not open")
        self.register(candidate.format_id)
        sequence = self._sequence[candidate.format_id]
        self._buffer.append(
            (
                self._keys[candidate.format_id],
                sequence,
                _pack_blob_id(candidate.blob_id),
                self._encoding_key(candidate.encoding),
                candidate.length,
                candidate.weight,
            )
        )
        if len(self._buffer) >= 8192:
            self._flush()
        self._sequence[candidate.format_id] += 1
        self._capacity[candidate.format_id] += candidate.length * candidate.weight

    def _encoding_key(self, encoding: str) -> int:
        if encoding in self._encodings:
            return self._encodings[encoding]
        assert self._connection is not None
        key = len(self._encodings)
        self._connection.execute("INSERT INTO encodings VALUES (?, ?)", (key, encoding))
        self._encodings[encoding] = key
        return key

    def register(self, format_id: str) -> None:
        if format_id not in self._keys:
            self._keys[format_id] = len(self._keys)
        self._sequence.setdefault(format_id, 0)
        self._capacity.setdefault(format_id, 0)
        self._exhausted.setdefault(format_id, False)

    def capacity(self, format_id: str) -> int:
        return self._capacity.get(format_id, 0)

    def candidates(self, format_id: str) -> int:
        return self._sequence.get(format_id, 0)

    def is_exhausted(self, format_id: str) -> bool:
        return self._exhausted.get(format_id, False)

    def set_exhausted(self, format_id: str) -> None:
        self._exhausted[format_id] = True

    def _flush(self) -> None:
        assert self._connection is not None
        self._connection.executemany(
            "INSERT INTO candidates VALUES (?, ?, ?, ?, ?, ?)", self._buffer
        )
        self._buffer.clear()

    def is_complete(self, config: str) -> bool:
        return config in self._completed

    def cursor(self, config: str) -> tuple[int, int]:
        return self._cursors.get(config, (0, 0))

    def finish_config(
        self, config: str, cursor: tuple[int, int] | None = None
    ) -> None:
        assert self._connection is not None
        self._flush()
        self._write_formats()
        cursor = cursor or self.cursor(config)
        self._connection.execute(
            "INSERT OR REPLACE INTO completed_configs VALUES (?, ?, ?)",
            (config, *cursor),
        )
        self._connection.commit()
        self._completed.add(config)
        self._cursors[config] = cursor

    def __exit__(self, exc_type, _exc, _traceback) -> None:
        if self._connection is None:
            return
        if exc_type is not None:
            self._connection.close()
            self._release_lock()
            return
        self._flush()
        self._write_formats()
        self._connection.commit()
        self._connection.close()
        os.replace(self._tmp, self.path)
        self._release_lock()

    def _release_lock(self) -> None:
        if self._lock_handle is not None:
            fcntl.flock(self._lock_handle, fcntl.LOCK_UN)
            self._lock_handle.close()
            self._lock_handle = None

    def _write_formats(self) -> None:
        assert self._connection is not None
        rows = [
            (
                self._keys[key],
                key,
                self._sequence[key],
                self._capacity[key],
                int(self._exhausted[key]),
            )
            for key in sorted(self._sequence)
        ]
        self._connection.executemany(
            "INSERT INTO formats VALUES (?, ?, ?, ?, ?) "
            "ON CONFLICT(format_key) DO UPDATE SET "
            "candidates=excluded.candidates, capacity=excluded.capacity, "
            "exhausted=excluded.exhausted",
            rows,
        )


class Manifest:
    def __init__(
        self,
        path: Path,
        revision: str,
        capacities: dict[str, int],
        counts: dict[str, int],
        keys: dict[str, int],
        exhausted: dict[str, bool],
        roster_hash: str,
    ) -> None:
        self.path = path
        self.revision = revision
        self._capacities = capacities
        self._counts = counts
        self._keys = keys
        self._exhausted = exhausted
        self.roster_hash = roster_hash

    def capacity(self, format_id: str) -> int:
        return self._capacities.get(format_id, 0)

    def exhausted(self, format_id: str) -> bool:
        return self._exhausted.get(format_id, False)

    def read(self, format_id: str, cursor: int, limit: int) -> ManifestBatch:
        with sqlite3.connect(self.path) as connection:
            rows = connection.execute(
                "SELECT c.blob_id, e.name, c.length, c.weight "
                "FROM candidates c JOIN encodings e ON e.key = c.encoding_key "
                "WHERE c.format_key = ? AND c.sequence >= ? "
                "ORDER BY c.sequence LIMIT ?",
                (self._keys[format_id], cursor, limit),
            ).fetchall()
        items = tuple(
            Candidate(format_id, _unpack_blob_id(row[0]), *row[1:]) for row in rows
        )
        next_cursor = cursor + len(items)
        count = self._counts.get(format_id, 0)
        return ManifestBatch(items, next_cursor, next_cursor >= count)


def open_manifest(path: Path, roster_hash: str) -> Manifest:
    """Open a complete manifest and verify its roster identity."""

    with sqlite3.connect(path) as connection:
        metadata = dict(connection.execute("SELECT key, value FROM metadata"))
        rows = connection.execute(
            "SELECT format_key, id, candidates, capacity, exhausted FROM formats"
        ).fetchall()
    if metadata.get("roster_hash") != roster_hash:
        raise ConfigurationError("manifest roster does not match this training run")
    capacities = {
        format_id: capacity for _key, format_id, _count, capacity, _done in rows
    }
    counts = {format_id: count for _key, format_id, count, _capacity, _done in rows}
    keys = {format_id: key for key, format_id, _count, _capacity, _done in rows}
    exhausted = {
        format_id: bool(done) for _key, format_id, _count, _capacity, done in rows
    }
    return Manifest(
        path,
        metadata["revision"],
        capacities,
        counts,
        keys,
        exhausted,
        metadata["roster_hash"],
    )


def _pack_blob_id(blob_id: str) -> bytes:
    if (
        len(blob_id) == 40
        and blob_id == blob_id.lower()
        and all(char in "0123456789abcdef" for char in blob_id)
    ):
        return b"\x00" + bytes.fromhex(blob_id)
    return b"\x01" + blob_id.encode()


def _unpack_blob_id(value: bytes) -> str:
    if value[:1] == b"\x00":
        return value[1:].hex()
    return value[1:].decode()


_SCHEMA = """
PRAGMA journal_mode = DELETE;
PRAGMA synchronous = NORMAL;
PRAGMA cache_size = -65536;
CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE completed_configs (
    config TEXT PRIMARY KEY,
    shard INTEGER NOT NULL,
    row INTEGER NOT NULL
);
CREATE TABLE formats (
    format_key INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    candidates INTEGER NOT NULL,
    capacity INTEGER NOT NULL,
    exhausted INTEGER NOT NULL
);
CREATE TABLE encodings (
    key INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);
CREATE TABLE candidates (
    format_key INTEGER NOT NULL,
    sequence INTEGER NOT NULL,
    blob_id BLOB NOT NULL,
    encoding_key INTEGER NOT NULL,
    length INTEGER NOT NULL,
    weight INTEGER NOT NULL,
    PRIMARY KEY (format_key, sequence)
) WITHOUT ROWID;
"""
