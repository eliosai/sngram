"""Atomic durable state for one streaming training run."""

from __future__ import annotations

import json
import os
import sqlite3
from dataclasses import dataclass, field
from pathlib import Path

import sngram

from .corpus import Tallies
from .errors import ConfigurationError

_VERSION = 9


@dataclass
class RunState:
    corpus: str
    stream_state: dict | None = None
    rows: int = 0
    vendor_files: int = 0
    decoded: int = 0
    shard_bytes: int = 0
    langs: dict[str, int] = field(default_factory=dict)
    tallies: Tallies = field(default_factory=Tallies)


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
            "INSERT INTO checkpoint VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            _record(counter, state),
        )
    os.replace(temporary, path)


def load(path: Path, corpus: str) -> tuple[sngram.BigramCounter, RunState]:
    """Load a checkpoint bound to this corpus, or return a fresh run."""

    if not path.exists():
        return sngram.BigramCounter(), RunState(corpus)
    with sqlite3.connect(path) as connection:
        row = connection.execute("SELECT * FROM checkpoint").fetchone()
    if row is None or (row[0], row[1]) != (_VERSION, corpus):
        raise ConfigurationError(_mismatch(row, corpus))
    counter = sngram.BigramCounter()
    counter.restore(row[4], row[5], row[6], row[7])
    return counter, _state(row[1], row[2], row[3])


_RESTART = "pass --no-resume or a fresh --mint-dir to restart"


def _mismatch(row: tuple | None, corpus: str) -> str:
    wanted = corpus.split("@")[0]
    if row is None or row[0] != _VERSION:
        return f"checkpoint predates this trainer and cannot resume {wanted!r}; {_RESTART}"
    held = str(row[1]).split("@")[0]
    if held != wanted:
        return f"checkpoint holds corpus {held!r}, this run wants {wanted!r}; {_RESTART}"
    return f"checkpoint for corpus {wanted!r} was written at another revision; {_RESTART}"


def _record(counter: sngram.BigramCounter, state: RunState) -> tuple[object, ...]:
    progress = {
        "rows": state.rows,
        "vendor_files": state.vendor_files,
        "decoded": state.decoded,
        "shard_bytes": state.shard_bytes,
        "langs": state.langs,
        "tallies": vars(state.tallies),
    }
    return (
        _VERSION,
        state.corpus,
        json.dumps(state.stream_state) if state.stream_state is not None else None,
        json.dumps(progress),
        counter.snapshot(),
        counter.pairs_processed,
        counter.bytes_processed,
        counter.files_processed,
    )


def _state(corpus: str, stream_json: str | None, progress_json: str) -> RunState:
    progress = json.loads(progress_json)
    return RunState(
        corpus,
        json.loads(stream_json) if stream_json is not None else None,
        progress["rows"],
        progress["vendor_files"],
        progress["decoded"],
        progress["shard_bytes"],
        dict(progress["langs"]),
        Tallies(**progress["tallies"]),
    )


_SCHEMA = """
CREATE TABLE checkpoint (
    version INTEGER NOT NULL,
    corpus TEXT NOT NULL,
    stream_json TEXT,
    state_json TEXT NOT NULL,
    counts BLOB NOT NULL,
    pairs INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    files INTEGER NOT NULL
)
"""
