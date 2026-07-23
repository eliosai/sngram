"""Durable sampled object manifest with stable per-format cursors."""

from __future__ import annotations

import os
import sqlite3
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
    extension: str = ""
    license: str = ""


@dataclass(frozen=True)
class ManifestBatch:
    items: tuple[Candidate, ...]
    cursor: int
    exhausted: bool


READ_AHEAD_ROWS = 1024


class ManifestWriter:
    """One-shot atomic manifest writer fed rows in per-format order."""

    def __init__(self, path: Path, revision: str, roster_hash: str) -> None:
        self.path = path
        self.revision = revision
        self.roster_hash = roster_hash
        self._tmp = path.with_suffix(path.suffix + ".tmp")
        self._connection: sqlite3.Connection | None = None
        self._sequence: dict[str, int] = defaultdict(int)
        self._capacity: dict[str, int] = defaultdict(int)
        self._keys: dict[str, int] = {}
        self._exhausted: dict[str, bool] = {}
        self._encodings: dict[str, int] = {}
        self._targets: dict[str, int] = {}

    def __enter__(self) -> ManifestWriter:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self._tmp.unlink(missing_ok=True)
        self._connection = sqlite3.connect(self._tmp)
        self._connection.executescript(_SCHEMA)
        self._connection.execute("PRAGMA journal_mode = OFF")
        self._connection.execute("PRAGMA synchronous = OFF")
        return self

    def register(self, format_id: str, exhausted: bool = False) -> None:
        if format_id not in self._keys:
            self._keys[format_id] = len(self._keys)
        self._exhausted[format_id] = exhausted or self._exhausted.get(format_id, False)

    def add_rows(self, format_id: str, rows) -> None:
        """Append ordered (blob_id, encoding, length, weight, extension, license) rows."""

        assert self._connection is not None
        packed, capacity = self._pack(self._keys[format_id], self._sequence[format_id], rows)
        self._connection.executemany(
            "INSERT INTO candidates VALUES (?, ?, ?, ?, ?, ?, ?, ?)", packed
        )
        self._sequence[format_id] += len(packed)
        self._capacity[format_id] += capacity

    def _pack(self, key: int, sequence: int, rows) -> tuple[list[tuple], int]:
        packed = []
        capacity = 0
        for blob_id, encoding, length, weight, extension, license in rows:
            packed.append(
                (key, sequence, _pack_blob_id(blob_id), self._encoding_key(encoding),
                 length, weight, extension, license)
            )
            capacity += length * weight
            sequence += 1
        return packed, capacity

    def _encoding_key(self, encoding: str) -> int:
        if encoding in self._encodings:
            return self._encodings[encoding]
        assert self._connection is not None
        key = len(self._encodings)
        self._connection.execute("INSERT INTO encodings VALUES (?, ?)", (key, encoding))
        self._encodings[encoding] = key
        return key

    def candidates(self, format_id: str) -> int:
        return self._sequence.get(format_id, 0)

    def set_targets(self, built: int | None, effective: int | None) -> None:
        if built is not None:
            self._targets["built_target"] = built
        if effective is not None:
            self._targets["effective_target"] = effective

    def __exit__(self, exc_type, _exc, _traceback) -> None:
        if self._connection is None:
            return
        if exc_type is not None:
            self._connection.close()
            self._tmp.unlink(missing_ok=True)
            return
        self._write_tables()
        self._connection.commit()
        self._connection.close()
        os.replace(self._tmp, self.path)

    def _write_tables(self) -> None:
        assert self._connection is not None
        metadata = {"revision": self.revision, "roster_hash": self.roster_hash}
        metadata.update({key: str(value) for key, value in self._targets.items()})
        self._connection.executemany(
            "INSERT INTO metadata VALUES (?, ?)", sorted(metadata.items())
        )
        self._connection.executemany(
            "INSERT INTO formats VALUES (?, ?, ?, ?, ?)",
            [
                (key, format_id, self._sequence[format_id], self._capacity[format_id],
                 int(self._exhausted[format_id]))
                for format_id, key in self._keys.items()
            ],
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
        effective_target: int | None = None,
    ) -> None:
        self.path = path
        self.revision = revision
        self._capacities = capacities
        self._counts = counts
        self._keys = keys
        self._exhausted = exhausted
        self.roster_hash = roster_hash
        self.effective_target = effective_target
        self._connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
        self._connection.execute("PRAGMA mmap_size = 17179869184")
        self._connection.execute("PRAGMA cache_size = -131072")
        self._windows: dict[str, tuple[int, list[Candidate]]] = {}

    def capacity(self, format_id: str) -> int:
        return self._capacities.get(format_id, 0)

    def exhausted(self, format_id: str) -> bool:
        return self._exhausted.get(format_id, False)

    def read(self, format_id: str, cursor: int, limit: int) -> ManifestBatch:
        items = tuple(self._window(format_id, cursor, limit))
        next_cursor = cursor + len(items)
        count = self._counts.get(format_id, 0)
        return ManifestBatch(items, next_cursor, next_cursor >= count)

    def _window(self, format_id: str, cursor: int, limit: int) -> list[Candidate]:
        start, rows = self._windows.get(format_id, (0, []))
        offset = cursor - start
        covered = offset + limit <= len(rows) or start + len(rows) >= self._counts.get(format_id, 0)
        if 0 <= offset <= len(rows) and covered:
            return rows[offset : offset + limit]
        rows = self._query(format_id, cursor, max(limit, READ_AHEAD_ROWS))
        self._windows[format_id] = (cursor, rows)
        return rows[:limit]

    def _query(self, format_id: str, cursor: int, limit: int) -> list[Candidate]:
        rows = self._connection.execute(
            "SELECT c.blob_id, e.name, c.length, c.weight "
            "FROM candidates c JOIN encodings e ON e.key = c.encoding_key "
            "WHERE c.format_key = ? AND c.sequence >= ? "
            "ORDER BY c.sequence LIMIT ?",
            (self._keys[format_id], cursor, limit),
        ).fetchall()
        return [Candidate(format_id, _unpack_blob_id(row[0]), *row[1:]) for row in rows]

    def close(self) -> None:
        self._connection.close()


def open_manifest(path: Path, roster_hash: str) -> Manifest:
    """Open a complete manifest and verify its roster identity."""

    with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as connection:
        metadata = dict(connection.execute("SELECT key, value FROM metadata"))
        rows = connection.execute(
            "SELECT format_key, id, candidates, capacity, exhausted FROM formats"
        ).fetchall()
    if metadata.get("roster_hash") != roster_hash:
        raise ConfigurationError("manifest roster does not match this training run")
    effective = metadata.get("effective_target")
    return Manifest(
        path,
        metadata["revision"],
        {format_id: capacity for _k, format_id, _n, capacity, _d in rows},
        {format_id: count for _k, format_id, count, _c, _d in rows},
        {format_id: key for key, format_id, _n, _c, _d in rows},
        {format_id: bool(done) for _k, format_id, _n, _c, done in rows},
        metadata["roster_hash"],
        int(effective) if effective is not None else None,
    )


def stored_format_ids(path: Path) -> list[str]:
    """List the format ids recorded in a complete manifest."""

    with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as connection:
        return [row[0] for row in connection.execute("SELECT id FROM formats ORDER BY id")]


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
    extension TEXT NOT NULL,
    license TEXT NOT NULL,
    PRIMARY KEY (format_key, sequence)
) WITHOUT ROWID;
"""
