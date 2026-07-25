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
        repo="org/dataset",
        stream_state={"cursor": 3, "partial": [[2, 1, 128]]},
        repos=42,
        vendor_files=2,
        decoded=9_000,
        shard_bytes=5_000,
        langs={"Rust": 7_000, "Python": 2_000},
    )

    save(tmp_path / "checkpoint.sqlite3", counter, state)
    restored_counter, restored = load(
        tmp_path / "checkpoint.sqlite3", "revision", "org/dataset"
    )

    assert restored_counter.snapshot() == counter.snapshot()
    assert restored == state


def test_checkpoint_rejects_identity_changes(tmp_path: Path):
    path = tmp_path / "checkpoint.sqlite3"
    save(path, sngram.BigramCounter(), RunState("rev", "org/a"))

    with pytest.raises(ConfigurationError, match="revision"):
        load(path, "other", "org/a")
    with pytest.raises(ConfigurationError, match="repo"):
        load(path, "rev", "org/b")


def test_missing_checkpoint_returns_fresh_state(tmp_path: Path):
    counter, state = load(tmp_path / "missing.sqlite3", "rev", "org/a")

    assert counter.bytes_processed == 0
    assert state == RunState("rev", "org/a")


def test_written_table_carries_the_provenance(tmp_path: Path):
    counter = sngram.BigramCounter()
    counter.process(b"fn main() {}")

    write_table(tmp_path, "final", counter, "stack-v3@abc 12 content bytes")

    table = sngram.WeightTable.from_path(tmp_path / "final_weights.bin")
    assert table.provenance == "stack-v3@abc 12 content bytes"
    assert table.version == 2
