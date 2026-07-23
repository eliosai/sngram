"""Atomic durable state for one streaming training run."""

from __future__ import annotations

import json
import os
import sqlite3
from dataclasses import dataclass, field
from pathlib import Path

import sngram

from .errors import ConfigurationError

_VERSION = 7


@dataclass
class RunState:
    revision: str
    corpus_id: str
    stream_state: dict | None = None
    rows: int = 0
    skips: int = 0
    fetched: int = 0
    groups: dict[str, int] = field(default_factory=dict)


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
    path: Path, revision: str, corpus_id: str
) -> tuple[sngram.BigramCounter, RunState]:
    """Load a matching checkpoint or return a fresh run."""

    if not path.exists():
        return sngram.BigramCounter(), RunState(revision, corpus_id)
    with sqlite3.connect(path) as connection:
        row = connection.execute("SELECT * FROM checkpoint").fetchone()
    if row is None or (row[0], row[1], row[2]) != (_VERSION, revision, corpus_id):
        raise ConfigurationError(
            "checkpoint does not match this corpus revision and identity; "
            "pass --no-resume or a fresh --mint-dir to restart"
        )
    counter = sngram.BigramCounter()
    counter.restore(row[5], row[6], row[7], row[8])
    return counter, _state(row[1], row[2], row[3], row[4])


def _record(counter: sngram.BigramCounter, state: RunState) -> tuple[object, ...]:
    progress = {
        "rows": state.rows,
        "skips": state.skips,
        "fetched": state.fetched,
        "groups": state.groups,
    }
    return (
        _VERSION,
        state.revision,
        state.corpus_id,
        json.dumps(state.stream_state) if state.stream_state is not None else None,
        json.dumps(progress),
        counter.snapshot(),
        counter.pairs_processed,
        counter.bytes_processed,
        counter.files_processed,
    )


def _state(
    revision: str, corpus_id: str, stream_json: str | None, progress_json: str
) -> RunState:
    progress = json.loads(progress_json)
    return RunState(
        revision,
        corpus_id,
        json.loads(stream_json) if stream_json is not None else None,
        progress["rows"],
        progress["skips"],
        progress["fetched"],
        dict(progress["groups"]),
    )


_SCHEMA = """
CREATE TABLE checkpoint (
    version INTEGER NOT NULL,
    revision TEXT NOT NULL,
    corpus_id TEXT NOT NULL,
    stream_json TEXT,
    state_json TEXT NOT NULL,
    counts BLOB NOT NULL,
    pairs INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    files INTEGER NOT NULL
)
"""
