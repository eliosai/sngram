"""The weighted deficit planner: it must drive the realized byte-blend toward
the family weights, pick the most-underserved family, and skip exhausted ones."""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

from sngram.train.config import Family, Source
from sngram.train.pipeline import (
    Trainer,
    estimated_family_bytes,
    normalized_weights,
    pick_family,
    resume_dispatched,
)
from sngram.train.units import parse_size


def test_normalized_weights_sum_to_one():
    fams = [
        Family(id="a", sources=(), weight=3.0),
        Family(id="b", sources=(), weight=1.0),
    ]
    w = normalized_weights(fams)
    assert w == {"a": 0.75, "b": 0.25}


def test_pick_seeds_highest_weight_when_nothing_counted():
    w = {"a": 0.2, "b": 0.5, "c": 0.3}
    assert pick_family(["a", "b", "c"], w, {}) == "b"
    assert pick_family(["a", "b", "c"], w, {"a": 0, "b": 0, "c": 0}) == "b"


def test_pick_most_underserved_family():
    w = {"a": 0.5, "b": 0.5}
    # a already has all the bytes -> b is furthest below target
    assert pick_family(["a", "b"], w, {"a": 100, "b": 0}) == "b"


def test_pick_respects_weight_ratio():
    w = {"code": 0.8, "text": 0.2}
    # exactly on target (80/20) -> tie broken to first max; push text under:
    assert pick_family(["code", "text"], w, {"code": 85, "text": 15}) == "text"
    # code under its 80% target -> code chosen
    assert pick_family(["code", "text"], w, {"code": 70, "text": 30}) == "code"


def test_pick_only_considers_live_families():
    w = {"a": 0.9, "b": 0.1}
    # 'a' is exhausted (not live) even though it's most underserved
    assert pick_family(["b"], w, {"a": 0, "b": 50}) == "b"


def test_estimated_bytes_counts_inflight_with_global_mean():
    # nothing completed yet: in-flight estimated at the global mean (1.0),
    # so estimate == dispatched count
    est = estimated_family_bytes(
        counted={}, completed={}, dispatched={"a": 3, "b": 1}
    )
    assert est == {"a": 3.0, "b": 1.0}


def test_estimated_bytes_uses_per_family_mean_shard_size():
    # 'a' has 2 completed shards totalling 200 bytes -> mean 100; 1 still
    # in-flight -> estimate 200 + 100 = 300. 'b' mean 50, 2 in-flight -> 50+100.
    est = estimated_family_bytes(
        counted={"a": 200, "b": 50},
        completed={"a": 2, "b": 1},
        dispatched={"a": 3, "b": 3},
    )
    assert est["a"] == 300.0
    assert est["b"] == 150.0


def test_resume_dispatched_seeds_from_completed_counts():
    # on resume `dispatched` must start at the restored completed counts, not 0,
    # so `dispatched - completed` measures POST-resume in-flight
    assert resume_dispatched({"code": 500, "text": 100}, ["code", "text", "new"]) == {
        "code": 500, "text": 100, "new": 0,
    }


def test_resume_inflight_estimate_is_live_not_clamped_to_zero():
    # THE BUG: with `dispatched` reset to 0 but `completed` restored to 500,
    # in_flight = max(0 - 500, 0) = 0 forever — the lag-aware estimate is dead
    # for the whole post-resume run. Seeding dispatched from completed fixes it.
    family_bytes = {"code": 5_000_000_000}
    family_done = {"code": 500}
    dispatched = resume_dispatched(family_done, ["code"])
    dispatched["code"] += 1  # one post-resume dispatch
    est = estimated_family_bytes(family_bytes, family_done, dispatched)
    # in_flight = 501 - 500 = 1 shard at mean 5e9/500 = 1e7
    assert est["code"] == 5_000_000_000 + 10_000_000


def test_failed_shards_do_not_inflate_inflight_estimate():
    # 20 'code' shards were dispatched, NONE completed, all 20 FAILED (e.g. a
    # 404'd/gated source). They contributed zero bytes, so in-flight must be 0 —
    # otherwise the planner believes a dead source is holding its share and the
    # blend silently goes wrong over a long run
    est = estimated_family_bytes(
        counted={"text": 200_000_000},
        completed={"text": 20},
        dispatched={"code": 20, "text": 20},
        failed={"code": 20},
    )
    assert est["code"] == 0.0


def test_inflight_estimate_prevents_dead_time_drift():
    # the planner has dispatched a burst of 'code' that hasn't counted yet;
    # the estimate must already reflect it, so the next pick is NOT more code
    weights = {"code": 0.8, "text": 0.2}
    est = estimated_family_bytes(
        counted={}, completed={}, dispatched={"code": 8, "text": 0}
    )
    # code is already over its share on committed work -> text is chosen
    assert pick_family(["code", "text"], weights, est) == "text"


# --- integration: the realized blend tracks the weights while data lasts ------

def _weighted_family(directory: Path, fid: str, weight: float, files: int) -> Family:
    directory.mkdir(parents=True, exist_ok=True)
    rows = ["y" * 2000] * 50  # ~100 KB per shard file
    for i in range(files):
        tbl = pa.table({"content": pa.array(rows, type=pa.large_string())})
        pq.write_table(tbl, directory / f"{fid}-{i}.parquet")
    glob = str(directory / f"{fid}-*.parquet")
    return Family(
        id=fid, weight=weight,
        sources=(Source(fid, "local", "content", data_files=glob),),
    )


def _capped_family(directory: Path, fid: str, cap_bytes: int, files: int) -> Family:
    directory.mkdir(parents=True, exist_ok=True)
    rows = ["x" * 1000] * 100  # ~100 KB per shard file
    for i in range(files):
        tbl = pa.table({"content": pa.array(rows, type=pa.large_string())})
        pq.write_table(tbl, directory / f"{fid}-{i}.parquet")
    glob = str(directory / f"{fid}-*.parquet")
    return Family(
        id=fid, weight=cap_bytes,
        cap_bytes=cap_bytes,
        sources=(Source(fid, "local", "content", data_files=glob, cap_bytes=cap_bytes),),
    )


def _capped_family_with_rows(
    directory: Path, fid: str, cap_bytes: int, rows: list[str], files: int
) -> Family:
    directory.mkdir(parents=True, exist_ok=True)
    for i in range(files):
        tbl = pa.table({"content": pa.array(rows, type=pa.large_string())})
        pq.write_table(tbl, directory / f"{fid}-{i}.parquet")
    glob = str(directory / f"{fid}-*.parquet")
    return Family(
        id=fid, weight=cap_bytes,
        cap_bytes=cap_bytes,
        sources=(Source(fid, "local", "content", data_files=glob, cap_bytes=cap_bytes),),
    )


def _source_capped_family(
    directory: Path, fid: str, family_cap: int, source_cap: int, files: int
) -> Family:
    directory.mkdir(parents=True, exist_ok=True)
    rows = ["x" * 1000] * 100
    for i in range(files):
        tbl = pa.table({"content": pa.array(rows, type=pa.large_string())})
        pq.write_table(tbl, directory / f"{fid}-{i}.parquet")
    glob = str(directory / f"{fid}-*.parquet")
    return Family(
        id=fid, weight=family_cap, cap_bytes=family_cap,
        sources=(Source(fid, "local", "content", data_files=glob, cap_bytes=source_cap),),
    )


def _write_source(directory: Path, name: str, rows: list[str], files: int) -> str:
    directory.mkdir(parents=True, exist_ok=True)
    for i in range(files):
        tbl = pa.table({"content": pa.array(rows, type=pa.large_string())})
        pq.write_table(tbl, directory / f"{name}-{i}.parquet")
    return str(directory / f"{name}-*.parquet")


def test_weighted_blend_holds_target_share(tmp_path: Path):
    # code weighted 0.8, text 0.2; both have far more data than the limit, so
    # neither exhausts and the realized blend must track ~0.8 / ~0.2
    code = _weighted_family(tmp_path / "code", "code", 0.8, files=60)
    text = _weighted_family(tmp_path / "text", "text", 0.2, files=60)
    trainer = Trainer(
        families=[code, text],
        mint_dir=tmp_path / "bins",
        target=parse_size("50TB"),
        mint_every=parse_size("50TB"),
        workers=2,
        limit=parse_size("2MB"),     # stops well before either family's ~6 MB
        checkpoint_every_s=3600.0,
        resume=False,
    )
    asyncio.run(trainer.run())
    fb = trainer.state.family_bytes
    total = sum(fb.values())
    code_share = fb.get("code", 0) / total
    assert 0.65 <= code_share <= 0.92, f"code share {code_share:.2%} off target 0.80"


def test_weighted_blend_holds_across_resume(tmp_path: Path):
    # the blend must keep tracking the weights AFTER a crash-resume, not just
    # within one process — exercises _plan across a real checkpoint boundary
    code = _weighted_family(tmp_path / "code", "code", 0.8, files=120)
    text = _weighted_family(tmp_path / "text", "text", 0.2, files=120)

    def run(limit: str, resume: bool) -> Trainer:
        t = Trainer(
            families=[code, text],
            mint_dir=tmp_path / "bins",
            target=parse_size("50TB"),
            mint_every=parse_size("50TB"),
            workers=2,
            limit=parse_size(limit),
            checkpoint_every_s=3600.0,
            resume=resume,
        )
        asyncio.run(t.run())
        return t

    run("1MB", False)        # partial run, checkpoints the blend feedback
    t2 = run("4MB", True)    # resume and continue
    fb = t2.state.family_bytes
    total = sum(fb.values())
    code_share = fb.get("code", 0) / total
    assert 0.62 <= code_share <= 0.92, f"blend broke across resume: {code_share:.2%}"


def test_sources_inside_family_are_round_robin(tmp_path: Path):
    rows = ["x" * 1000] * 100
    a = _write_source(tmp_path / "a", "a", rows, files=3)
    b = _write_source(tmp_path / "b", "b", rows, files=3)
    family = Family(
        id="blend",
        weight=1.0,
        cap_bytes=600_000,
        sources=(
            Source("blend", "local-a", "content", data_files=a, cap_bytes=300_000),
            Source("blend", "local-b", "content", data_files=b, cap_bytes=300_000),
        ),
    )
    trainer = Trainer(
        families=[family],
        mint_dir=tmp_path / "bins",
        target=parse_size("50TB"),
        mint_every=parse_size("50TB"),
        workers=1,
        limit=parse_size("200KB"),
        checkpoint_every_s=3600.0,
        resume=False,
    )
    asyncio.run(trainer.run())

    assert trainer.state.source_bytes["blend/local-a"] == 100_000
    assert trainer.state.source_bytes["blend/local-b"] == 100_000


def test_caps_stop_multilingual_after_code_exhausts(tmp_path: Path):
    code = _capped_family(tmp_path / "code", "code", cap_bytes=100_000, files=1)
    multilingual = _capped_family(
        tmp_path / "multilingual", "multilingual", cap_bytes=200_000, files=10
    )
    trainer = Trainer(
        families=[code, multilingual],
        mint_dir=tmp_path / "bins",
        target=parse_size("50TB"),
        mint_every=parse_size("50TB"),
        workers=1,
        limit=None,
        checkpoint_every_s=3600.0,
        resume=False,
    )
    asyncio.run(trainer.run())

    assert trainer.state.family_bytes["code"] <= code.cap_bytes
    assert trainer.state.family_bytes["multilingual"] <= multilingual.cap_bytes
    assert trainer.state.family_bytes["multilingual"] == 200_000


def test_caps_hold_across_resume(tmp_path: Path):
    multilingual = _capped_family(
        tmp_path / "multilingual", "multilingual", cap_bytes=200_000, files=10
    )

    def run(limit: str | None, resume: bool) -> Trainer:
        trainer = Trainer(
            families=[multilingual],
            mint_dir=tmp_path / "bins",
            target=parse_size("50TB"),
            mint_every=parse_size("50TB"),
            workers=1,
            limit=parse_size(limit) if limit else None,
            checkpoint_every_s=3600.0,
            resume=resume,
        )
        asyncio.run(trainer.run())
        return trainer

    run("100KB", False)
    resumed = run(None, True)

    assert resumed.state.family_bytes["multilingual"] == 200_000
    assert resumed.state.source_bytes["multilingual/local"] == 200_000


def test_source_cap_stops_dispatching_source(tmp_path: Path):
    family = _source_capped_family(
        tmp_path / "source", "docs", family_cap=1_000_000, source_cap=200_000, files=10
    )
    trainer = Trainer(
        families=[family],
        mint_dir=tmp_path / "bins",
        target=parse_size("50TB"),
        mint_every=parse_size("50TB"),
        workers=1,
        limit=None,
        checkpoint_every_s=3600.0,
        resume=False,
    )
    asyncio.run(trainer.run())

    assert trainer.state.source_bytes["docs/local"] == 200_000
    assert len(trainer.state.completed["docs/local"]["done"]) == 2
    events = [
        json.loads(line)
        for line in (tmp_path / "bins" / "train-events.jsonl").read_text().splitlines()
    ]
    assert sum(e.get("kind") == "source_cap_reached" for e in events) <= 1
    assert sum(e.get("kind") == "family_done" for e in events) <= 1


def test_oversized_shard_fills_to_cap_without_overshoot(tmp_path: Path):
    family = _capped_family(tmp_path / "huge", "multilingual", cap_bytes=50_000, files=1)
    trainer = Trainer(
        families=[family],
        mint_dir=tmp_path / "bins",
        target=parse_size("50TB"),
        mint_every=parse_size("50TB"),
        workers=1,
        limit=None,
        checkpoint_every_s=3600.0,
        resume=False,
    )
    asyncio.run(trainer.run())

    assert trainer.state.family_bytes["multilingual"] == 50_000
    assert trainer.durable_bytes() == 50_000


def test_single_large_row_fills_cap_without_overshoot(tmp_path: Path):
    family = _capped_family_with_rows(
        tmp_path / "large-row", "multilingual", cap_bytes=500, rows=["x" * 1000], files=1
    )
    trainer = Trainer(
        families=[family],
        mint_dir=tmp_path / "bins",
        target=parse_size("50TB"),
        mint_every=parse_size("50TB"),
        workers=1,
        limit=None,
        checkpoint_every_s=3600.0,
        resume=False,
    )
    asyncio.run(trainer.run())

    assert trainer.state.family_bytes["multilingual"] == 500
    assert trainer.durable_bytes() == 500


def test_parallel_final_shards_fill_cap_without_inflight_overshoot(tmp_path: Path):
    family = _capped_family(tmp_path / "parallel", "multilingual", cap_bytes=150_000, files=6)
    trainer = Trainer(
        families=[family],
        mint_dir=tmp_path / "bins",
        target=parse_size("50TB"),
        mint_every=parse_size("50TB"),
        workers=4,
        limit=None,
        checkpoint_every_s=3600.0,
        resume=False,
    )
    asyncio.run(trainer.run())

    assert trainer.state.family_bytes["multilingual"] == 150_000
    assert trainer.state.source_bytes["multilingual/local"] == 150_000
    assert trainer.durable_bytes() == 150_000
