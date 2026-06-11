"""Checkpoint round-trip: a restored run equals an uninterrupted one."""

from pathlib import Path

import sngram
from sngram.train import checkpoint


def make_counter(rows: list[bytes]) -> sngram.BigramCounter:
    c = sngram.BigramCounter()
    for r in rows:
        c.process(r)
    c.add_files(len(rows))
    return c


def test_snapshot_restore_round_trip(tmp_path: Path):
    rows = [b"fn main() {}", b"the quick brown fox", b"zq" * 50]
    original = make_counter(rows)

    state = checkpoint.RunState()
    state.mark_done("the-stack", 100, 3, "sha1")
    state.mark_done("the-stack", 100, 7, "sha1")
    state.mark_done("fineweb-2/cmn_Hani", 12, 0, None)
    state.mints_done.append("5tb")
    state.revisions["bigcode/the-stack"] = "sha1"

    checkpoint.save(tmp_path, original, state)

    restored_counter = sngram.BigramCounter()
    restored = checkpoint.load(tmp_path, restored_counter)
    assert restored is not None

    # counts identical, pair for pair
    for a in range(0, 256, 7):
        for b in range(0, 256, 11):
            assert restored_counter.count(a, b) == original.count(a, b)
    assert restored_counter.pairs_processed == original.pairs_processed
    assert restored_counter.bytes_processed == original.bytes_processed
    assert restored_counter.files_processed == original.files_processed

    # completed-shard semantics survive, including the n_shards + revision guards
    assert restored.is_done("the-stack", 100, 3, "sha1")
    assert restored.is_done("the-stack", 100, 7, "sha1")
    assert not restored.is_done("the-stack", 100, 4, "sha1")
    assert not restored.is_done("the-stack", 99, 3, "sha1"), "layout change invalidates"
    assert not restored.is_done("the-stack", 100, 3, "sha2"), "revision change invalidates"
    assert restored.is_done("fineweb-2/cmn_Hani", 12, 0, None)
    assert restored.mints_done == ["5tb"]
    assert restored.revisions == {"bigcode/the-stack": "sha1"}

    # the restored counter mints the identical table
    assert restored_counter.to_table_bytes() == original.to_table_bytes()


def test_load_missing_returns_none(tmp_path: Path):
    assert checkpoint.load(tmp_path, sngram.BigramCounter()) is None


def test_save_is_idempotent_and_overwrites(tmp_path: Path):
    c = make_counter([b"abc"])
    state = checkpoint.RunState()
    checkpoint.save(tmp_path, c, state)
    c.process(b"def")
    state.mark_done("x", 1, 0, None)
    checkpoint.save(tmp_path, c, state)

    fresh = sngram.BigramCounter()
    restored = checkpoint.load(tmp_path, fresh)
    assert fresh.bytes_processed == 6
    assert restored.is_done("x", 1, 0, None)
