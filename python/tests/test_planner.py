"""The weighted deficit planner: it must drive the realized byte-blend toward
the family weights, pick the most-underserved family, and skip exhausted ones."""

from __future__ import annotations

import asyncio
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
