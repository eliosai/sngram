"""EventLog splits its JSONL into many small segments, losing nothing.

A 50 TB run logs for days; one ever-growing file is unreadable and risky.
The log is split into small, sequentially numbered segments — all retained as
debug material — and the active file is always the base path.
"""

import json
from pathlib import Path

from sngram.train.events import EventLog


def _read_all(path: Path) -> list[dict]:
    """Every event across every segment, oldest first."""
    rows: list[dict] = []
    for seg in EventLog.segment_paths(path):
        for line in seg.read_text().splitlines():
            if line.strip():
                rows.append(json.loads(line))
    return rows


def _fill(path: Path, n: int, segment_bytes: int = 1000) -> EventLog:
    log = EventLog(path, segment_bytes=segment_bytes)
    for i in range(n):
        log.log("shard", shard=f"s#{i}", i=i)
    log.close()
    return log


def test_splits_into_many_small_segments(tmp_path: Path):
    path = tmp_path / "train-events.jsonl"
    _fill(path, 500)
    segs = EventLog.segment_paths(path)
    assert len(segs) > 5, "should split into many files, not one giant log"
    assert path in segs, "the active file is always the base path"


def test_no_segment_exceeds_its_limit(tmp_path: Path):
    path = tmp_path / "train-events.jsonl"
    seg_bytes = 1000
    _fill(path, 500, segment_bytes=seg_bytes)
    for seg in EventLog.segment_paths(path):
        # a segment may overshoot only by the single line that tripped rotation
        assert seg.stat().st_size <= seg_bytes + 512, f"{seg.name} too big"


def test_all_events_preserved_in_order(tmp_path: Path):
    path = tmp_path / "train-events.jsonl"
    _fill(path, 500)
    rows = _read_all(path)
    assert [r["i"] for r in rows] == list(range(500)), "no event lost, order kept"


def test_segments_numbered_sequentially(tmp_path: Path):
    path = tmp_path / "train-events.jsonl"
    _fill(path, 500)
    archives = [p for p in EventLog.segment_paths(path) if p != path]
    nums = [int(p.name.split(".")[1]) for p in archives]
    assert nums == list(range(1, len(nums) + 1)), "archives are 1..N, contiguous"


def test_resume_continues_numbering_without_clobber(tmp_path: Path):
    path = tmp_path / "train-events.jsonl"
    _fill(path, 300)
    before = {p.name for p in EventLog.segment_paths(path) if p != path}

    # a restarted run must append, never overwrite an existing archive
    log = EventLog(path, segment_bytes=1000)
    for i in range(300, 600):
        log.log("shard", shard=f"s#{i}", i=i)
    log.close()

    after = {p.name for p in EventLog.segment_paths(path) if p != path}
    assert before <= after, "prior archives are preserved across a restart"
    assert [r["i"] for r in _read_all(path)] == list(range(600))


def test_checkpoint_excluded_from_dashboard_tail(tmp_path: Path):
    path = tmp_path / "train-events.jsonl"
    log = EventLog(path)
    log.log("checkpoint", bytes=1)
    log.log("error", stage="x")
    log.log("mint", label="100gb")
    kinds = [e["kind"] for e in log.tail]
    log.close()
    assert "checkpoint" not in kinds, "checkpoint status lives in the header now"
    assert "error" in kinds and "mint" in kinds, "real events still surface"
