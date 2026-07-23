"""Atomic durable state for one balanced training run."""

from __future__ import annotations

import json
import os
import sqlite3
from dataclasses import asdict, dataclass, field
from pathlib import Path

import sngram

from .errors import ConfigurationError

_VERSION = 5


@dataclass(frozen=True)
class FormatProgress:
    cursor: int = 0
    offset: int = 0
    effective_bytes: int = 0
    fetched_bytes: int = 0
    objects: int = 0
    exhausted: bool = False


_EMPTY_PROGRESS = FormatProgress()


@dataclass
class RunState:
    roster_hash: str
    revision: str
    target: int
    formats: dict[str, FormatProgress] = field(default_factory=dict)

    def progress(self, format_id: str) -> FormatProgress:
        return self.formats.get(format_id, _EMPTY_PROGRESS)


def write_table(
    mint_dir: Path, label: str, counter: sngram.BigramCounter, provenance: str
) -> None:
    """Atomically write one minted weight table with its provenance record."""

    mint_dir.mkdir(parents=True, exist_ok=True)
    table = sngram.WeightTable.from_bytes(counter.to_table_bytes())
    stamped = table.with_provenance(provenance)
    path = mint_dir / f"{label}_weights.bin"
    temporary = path.with_suffix(".bin.tmp")
    temporary.write_bytes(stamped.to_bytes())
    os.replace(temporary, path)


def save(path: Path, counter: sngram.BigramCounter, state: RunState) -> None:
    """Replace the checkpoint with one complete SQLite snapshot."""

    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.unlink(missing_ok=True)
    with sqlite3.connect(temporary) as connection:
        connection.execute(_SCHEMA)
        connection.execute(
            "INSERT INTO checkpoint VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            _record(counter, state),
        )
    os.replace(temporary, path)


def load(
    path: Path, roster_hash: str, target: int, revision: str = ""
) -> tuple[sngram.BigramCounter, RunState]:
    """Load a matching checkpoint or return a fresh run."""

    if not path.exists():
        return sngram.BigramCounter(), RunState(roster_hash, revision, target)
    with sqlite3.connect(path) as connection:
        row = connection.execute("SELECT * FROM checkpoint").fetchone()
    identity = (_VERSION, roster_hash, target)
    if row is None or (row[0], row[1], row[3]) != identity:
        raise ConfigurationError(
            "checkpoint does not match this roster and target; "
            "pass --no-resume or a fresh --mint-dir to restart"
        )
    counter = sngram.BigramCounter()
    counter.restore(row[5], row[6], row[7], row[8])
    return counter, _state(row[1], row[2], row[3], row[4])


def _record(counter: sngram.BigramCounter, state: RunState) -> tuple[object, ...]:
    formats = {key: asdict(value) for key, value in state.formats.items()}
    return (
        _VERSION,
        state.roster_hash,
        state.revision,
        state.target,
        json.dumps(formats),
        counter.snapshot(),
        counter.pairs_processed,
        counter.bytes_processed,
        counter.files_processed,
    )


def _state(roster_hash: str, revision: str, target: int, payload: str) -> RunState:
    formats = {key: FormatProgress(**value) for key, value in json.loads(payload).items()}
    return RunState(roster_hash, revision, target, formats)


_SCHEMA = """
CREATE TABLE checkpoint (
    version INTEGER NOT NULL,
    roster_hash TEXT NOT NULL,
    revision TEXT NOT NULL,
    target INTEGER NOT NULL,
    state_json TEXT NOT NULL,
    counts BLOB NOT NULL,
    pairs INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    files INTEGER NOT NULL
)
"""
