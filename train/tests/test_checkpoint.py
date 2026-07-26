from pathlib import Path

import pytest
import sngram

from sngram_train.checkpoint import RunState, load, save, write_table
from sngram_train.corpus import Reading, Shard, Tallies
from sngram_train.errors import ConfigurationError

BLEND = "blend@0123456789abcdef"

SHARD = Shard("p", 1, Reading("parquet-column", "text"), "fam", "src")


def test_checkpoint_round_trips_counter_and_stream_state(tmp_path: Path):
    counter = sngram.BigramCounter()
    counter.process(b"fn main() {}")
    tallies = Tallies()
    tallies.add(SHARD, 900)
    tallies.finish(SHARD)
    state = RunState(
        corpus=BLEND,
        stream_state={"cursors": {"src": 3}, "partial": [[2, 1, 128]]},
        rows=42,
        vendor_files=2,
        decoded=9_000,
        shard_bytes=5_000,
        langs={"Rust": 7_000, "Python": 2_000},
        tallies=tallies,
    )

    save(tmp_path / "checkpoint.sqlite3", counter, state)
    restored_counter, restored = load(tmp_path / "checkpoint.sqlite3", BLEND)

    assert restored_counter.snapshot() == counter.snapshot()
    assert restored == state
    assert restored.tallies.family_bytes == {"fam": 900}


def test_a_checkpoint_refuses_to_resume_into_a_different_corpus(tmp_path: Path):
    path = tmp_path / "checkpoint.sqlite3"
    save(path, sngram.BigramCounter(), RunState(BLEND))

    with pytest.raises(ConfigurationError, match="corpus 'blend'"):
        load(path, "stack-v3@0123456789abcdef")


def test_a_checkpoint_refuses_to_resume_at_a_different_revision(tmp_path: Path):
    path = tmp_path / "checkpoint.sqlite3"
    save(path, sngram.BigramCounter(), RunState(BLEND))

    with pytest.raises(ConfigurationError, match="another revision"):
        load(path, "blend@fedcba9876543210")


def test_missing_checkpoint_returns_fresh_state(tmp_path: Path):
    counter, state = load(tmp_path / "missing.sqlite3", BLEND)

    assert counter.bytes_processed == 0
    assert state == RunState(BLEND)


def test_written_table_carries_the_provenance(tmp_path: Path):
    counter = sngram.BigramCounter()
    counter.process(b"fn main() {}")

    write_table(tmp_path, "final", counter, "blend@abc 12 content bytes")

    table = sngram.WeightTable.from_path(tmp_path / "final_weights.bin")
    assert table.provenance == "blend@abc 12 content bytes"
    assert table.version == 2
