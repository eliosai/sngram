from pathlib import Path

import pytest
import sngram

from sngram_train.checkpoint import RunState, load, save, write_table
from sngram_train.errors import ConfigurationError


def test_checkpoint_round_trips_counter_and_stream_state(tmp_path: Path):
    counter = sngram.BigramCounter()
    counter.process(b"fn main() {}")
    state = RunState(
        revision="revision",
        stream_state={"shard": 3, "offset": 512},
        rows=42,
        skips=2,
        fetched=9_000,
        groups={"code": 7_000, "docs": 2_000},
    )

    save(tmp_path / "checkpoint.sqlite3", counter, state)
    restored_counter, restored = load(tmp_path / "checkpoint.sqlite3", "revision")

    assert restored_counter.snapshot() == counter.snapshot()
    assert restored == state


def test_checkpoint_rejects_a_revision_change(tmp_path: Path):
    path = tmp_path / "checkpoint.sqlite3"
    save(path, sngram.BigramCounter(), RunState("old"))

    with pytest.raises(ConfigurationError, match="revision"):
        load(path, "new")


def test_missing_checkpoint_returns_fresh_state(tmp_path: Path):
    counter, state = load(tmp_path / "missing.sqlite3", "rev")

    assert counter.bytes_processed == 0
    assert state == RunState("rev")


def test_written_table_carries_the_provenance(tmp_path: Path):
    counter = sngram.BigramCounter()
    counter.process(b"fn main() {}")

    write_table(tmp_path, "final", counter, "stack-v2@abc 12 effective bytes")

    table = sngram.WeightTable.from_path(tmp_path / "final_weights.bin")
    assert table.provenance == "stack-v2@abc 12 effective bytes"
    assert table.version == 2
