from pathlib import Path

import sngram

from sngram_train.checkpoint import FormatProgress, RunState, load, save


def test_checkpoint_round_trips_counter_and_format_progress(tmp_path: Path):
    counter = sngram.BigramCounter()
    counter.process(b"fn main() {}")
    state = RunState(
        roster_hash="roster",
        revision="revision",
        target=10_000,
        formats={
            "core/Python": FormatProgress(
                cursor=7,
                effective_bytes=12,
                fetched_bytes=9,
                objects=3,
                exhausted=True,
            )
        },
        mints_done=["100gb"],
    )

    save(tmp_path / "checkpoint.sqlite3", counter, state)
    restored_counter, restored = load(tmp_path / "checkpoint.sqlite3", "roster", 10_000)

    assert restored_counter.snapshot() == counter.snapshot()
    assert restored.formats == state.formats
    assert restored.mints_done == ["100gb"]
    assert restored.revision == "revision"


def test_checkpoint_rejects_roster_or_target_changes(tmp_path: Path):
    path = tmp_path / "checkpoint.sqlite3"
    save(path, sngram.BigramCounter(), RunState("old", "revision", 100))

    for roster, target in (("new", 100), ("old", 200)):
        try:
            load(path, roster, target)
        except RuntimeError as error:
            assert "checkpoint" in str(error)
        else:
            raise AssertionError("changed run identity should be rejected")


def test_missing_checkpoint_returns_fresh_state(tmp_path: Path):
    counter, state = load(tmp_path / "missing.sqlite3", "roster", 42, revision="rev")

    assert counter.bytes_processed == 0
    assert state == RunState("roster", "rev", 42)
