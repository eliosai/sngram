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
    state.mark_done("code-fixture", 100, 3, "sha1")
    state.mark_done("code-fixture", 100, 7, "sha1")
    state.mark_done("fineweb-2/cmn_Hani", 12, 0, None)
    state.mints_done.append("5tb")
    state.revisions["example/code-fixture"] = "sha1"
    state.roster_hash = "roster-sha"

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
    assert restored.is_done("code-fixture", 100, 3, "sha1")
    assert restored.is_done("code-fixture", 100, 7, "sha1")
    assert not restored.is_done("code-fixture", 100, 4, "sha1")
    assert not restored.is_done("code-fixture", 99, 3, "sha1"), "layout change invalidates"
    assert not restored.is_done("code-fixture", 100, 3, "sha2"), "revision change invalidates"
    assert restored.is_done("fineweb-2/cmn_Hani", 12, 0, None)
    assert restored.mints_done == ["5tb"]
    assert restored.revisions == {"example/code-fixture": "sha1"}
    assert restored.roster_hash == "roster-sha"

    # the restored counter mints the identical table
    assert restored_counter.to_table_bytes() == original.to_table_bytes()


def test_blend_feedback_survives_checkpoint(tmp_path: Path):
    # the weighted planner's per-family byte/shard feedback and the KL baseline
    # must persist, or a resumed run rebalances the blend from amnesia and loses
    # the convergence signal
    c = make_counter([b"abc"])
    state = checkpoint.RunState()
    state.family_bytes = {"code-github-2025": 3_000, "multilingual": 1_000}
    state.family_done = {"code-github-2025": 30, "multilingual": 10}
    state.source_bytes = {"multilingual/jpn_Jpan": 500}
    state.source_done = {"multilingual/jpn_Jpan": 5}
    state.last_mint_counts = [0] * (256 * 256)
    state.last_mint_counts[(ord("a") << 8) | ord("b")] = 42

    checkpoint.save(tmp_path, c, state)
    restored = checkpoint.load(tmp_path, sngram.BigramCounter())

    assert restored.family_bytes == {"code-github-2025": 3_000, "multilingual": 1_000}
    assert restored.family_done == {"code-github-2025": 30, "multilingual": 10}
    assert restored.source_bytes == {"multilingual/jpn_Jpan": 500}
    assert restored.source_done == {"multilingual/jpn_Jpan": 5}
    assert restored.last_mint_counts is not None
    assert len(restored.last_mint_counts) == 256 * 256
    assert restored.last_mint_counts[(ord("a") << 8) | ord("b")] == 42


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
