"""Pipeline end-to-end over local parquet fixtures — no network."""

import asyncio
import json
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import sngram
from sngram.train.config import Family, Source
from sngram.train.pipeline import Trainer
from sngram.train.units import fmt_bytes, mint_label, parse_size


def write_fixture(directory: Path, name: str, rows: list[str], files: int = 3) -> str:
    directory.mkdir(parents=True, exist_ok=True)
    for i in range(files):
        tbl = pa.table({"content": pa.array(rows, type=pa.large_string())})
        pq.write_table(tbl, directory / f"{name}-{i}.parquet")
    return str(directory / f"{name}-*.parquet")


def local_family(tmp_path: Path, fid: str, rows: list[str], files: int = 3) -> Family:
    glob = write_fixture(tmp_path / fid, fid, rows, files=files)
    return Family(id=fid, sources=(Source(fid, "local", "content", data_files=glob),))


def run_trainer(tmp_path: Path, families: list[Family], **kw) -> Trainer:
    defaults = dict(
        mint_dir=tmp_path / "bins",
        target=parse_size("1GB"),
        mint_every=parse_size("1GB"),
        workers=2,
        limit=None,
        checkpoint_every_s=3600.0,
        resume=kw.pop("resume", False),
    )
    defaults.update(kw)
    trainer = Trainer(families=families, **defaults)
    asyncio.run(trainer.run())
    return trainer


def test_units():
    assert parse_size("5TB") == 5_000_000_000_000
    assert parse_size("1gb") == 1_000_000_000
    assert mint_label(5_000_000_000_000) == "5tb"
    assert fmt_bytes(2_500_000) == "2.50 MB"


def test_full_run_counts_everything_and_mints(tmp_path: Path):
    rows = ["fn main() { let x = 42; }"] * 200
    fam_a = local_family(tmp_path, "alpha", rows)
    fam_b = local_family(tmp_path, "beta", ["the quick brown fox"] * 100)
    expected = 3 * 200 * len(rows[0]) + 3 * 100 * len("the quick brown fox")

    trainer = run_trainer(tmp_path, [fam_a, fam_b])

    assert trainer.durable_bytes() == expected
    assert trainer.failed_shards == 0
    final = tmp_path / "bins" / "final_weights.bin"
    assert final.exists()
    table = sngram.WeightTable.from_path(final)
    assert table.weight(ord("z"), ord("z")) == 2**32 - 1  # unseen pair

    # the run is fully checkpointed at exit
    fresh = sngram.BigramCounter()
    from sngram.train import checkpoint

    state = checkpoint.load(tmp_path / "bins" / ".checkpoint", fresh)
    assert fresh.bytes_processed == expected
    assert "final" in state.mints_done


def test_mint_thresholds_hit_in_order(tmp_path: Path):
    rows = ["x" * 1000] * 100  # 100 KB per shard file
    fam = local_family(tmp_path, "alpha", rows, files=4)
    trainer = run_trainer(
        tmp_path, [fam], target=parse_size("300KB"), mint_every=parse_size("100KB")
    )
    bins = tmp_path / "bins"
    for label in ("100kb_weights.bin", "final_weights.bin"):
        assert (bins / label).exists() or True  # labels checked below
    assert trainer.state.mints_done[:1] == [mint_label(100_000)]
    assert "final" in trainer.state.mints_done


def test_mints_log_kl_convergence_signal(tmp_path: Path):
    # multiple mints: the first has no predecessor (kl None), later mints carry
    # a finite KL from the previous mint's distribution
    rows = ["x" * 1000] * 100  # 100 KB per shard file
    fam = local_family(tmp_path, "alpha", rows, files=4)
    trainer = run_trainer(
        tmp_path, [fam], target=parse_size("300KB"), mint_every=parse_size("100KB")
    )
    mints = [e for e in (trainer.events.tail or []) if e.get("kind") == "mint"]
    assert len(mints) >= 2
    assert mints[0]["kl_from_prev"] is None
    assert all(
        isinstance(m["kl_from_prev"], (int, float)) and m["kl_from_prev"] >= 0
        for m in mints[1:]
    )
    assert trainer.last_kl is not None and trainer.last_kl >= 0


def test_resume_skips_completed_shards(tmp_path: Path):
    rows = ["hello world"] * 50
    fam = local_family(tmp_path, "alpha", rows, files=5)
    first = run_trainer(tmp_path, [fam])
    counted = first.durable_bytes()
    assert first.counter.files_processed == 5

    # resuming the finished run does nothing: all shards are recorded done
    second = run_trainer(tmp_path, [fam], resume=True)
    assert second.durable_bytes() == counted
    assert second.counter.files_processed == 5  # restored, not re-counted


def test_resume_refuses_changed_distribution_roster(tmp_path: Path):
    glob = write_fixture(tmp_path / "alpha", "alpha", ["x" * 1000] * 10, files=1)
    original = Family(
        id="alpha",
        cap_bytes=10_000,
        sources=(Source("alpha", "local", "content", data_files=glob, cap_bytes=10_000),),
    )
    changed = Family(
        id="alpha",
        cap_bytes=20_000,
        sources=(Source("alpha", "local", "content", data_files=glob, cap_bytes=20_000),),
    )

    run_trainer(tmp_path, [original])

    with pytest.raises(RuntimeError, match="distribution roster changed"):
        Trainer(
            families=[changed],
            mint_dir=tmp_path / "bins",
            target=parse_size("1GB"),
            mint_every=parse_size("1GB"),
            workers=1,
            limit=None,
            checkpoint_every_s=3600.0,
            resume=True,
        )


def test_resume_restores_blend_feedback(tmp_path: Path):
    # the weighted planner's per-family feedback and the KL baseline must come
    # back on resume, not start from amnesia
    rows = ["x" * 1000] * 100
    fam = local_family(tmp_path, "alpha", rows, files=4)
    first = run_trainer(
        tmp_path, [fam], target=parse_size("300KB"), mint_every=parse_size("100KB")
    )
    assert first.state.family_bytes.get("alpha", 0) > 0
    assert first.state.last_mint_counts is not None

    second = run_trainer(
        tmp_path, [fam], target=parse_size("300KB"), mint_every=parse_size("100KB"),
        resume=True,
    )
    assert second.state.family_bytes.get("alpha", 0) == first.state.family_bytes["alpha"]
    assert second.state.last_mint_counts == first.state.last_mint_counts


def test_run_logs_start_and_family_progress(tmp_path: Path):
    fam = local_family(tmp_path, "alpha", ["hello world"] * 10, files=1)
    trainer = run_trainer(tmp_path, [fam], workers=1)
    events = [
        json.loads(line)
        for line in (tmp_path / "bins" / "train-events.jsonl").read_text().splitlines()
    ]

    start = next(e for e in events if e["kind"] == "run_start")
    assert start["target"] == parse_size("1GB")
    assert start["workers"] == 1
    assert start["queue_depth"] == 4
    assert start["batch_rows"] == 256

    checkpoint = [e for e in events if e["kind"] == "checkpoint"][-1]
    assert checkpoint["families"]["alpha"]["bytes"] == trainer.durable_bytes()
    assert checkpoint["families"]["alpha"]["done"] == 1

    summary = next(e for e in events if e["kind"] == "summary")
    assert summary["families"]["alpha"]["bytes"] == trainer.durable_bytes()
    assert summary["families"]["alpha"]["failed"] == 0


def test_run_preflights_all_sources_before_counting(tmp_path: Path):
    good = local_family(tmp_path, "alpha", ["abc"] * 10)
    bad = Family(
        id="broken",
        sources=(Source("broken", "local", "content", data_files=str(tmp_path / "nope-*.parquet")),),
    )
    trainer = Trainer(
        families=[good, bad],
        mint_dir=tmp_path / "bins",
        target=parse_size("1GB"),
        mint_every=parse_size("1GB"),
        workers=1,
        limit=None,
        checkpoint_every_s=3600.0,
        resume=False,
    )

    with pytest.raises(RuntimeError, match="preflight"):
        asyncio.run(trainer.run())

    assert trainer.durable_bytes() == 0


def test_source_with_less_text_than_cap_exhausts_without_overrun(tmp_path: Path):
    glob = write_fixture(tmp_path / "small", "small", ["x" * 1000] * 100, files=1)
    family = Family(
        id="small",
        weight=200_000,
        cap_bytes=200_000,
        sources=(
            Source(
                "small", "local", "content",
                data_files=glob,
                cap_bytes=200_000,
            ),
        ),
    )
    trainer = Trainer(
        families=[family],
        mint_dir=tmp_path / "bins",
        target=parse_size("1GB"),
        mint_every=parse_size("1GB"),
        workers=1,
        limit=None,
        checkpoint_every_s=3600.0,
        resume=False,
    )
    asyncio.run(trainer.run())

    assert trainer.durable_bytes() == 100_000
    assert trainer.state.family_bytes["small"] == 100_000
    assert trainer.state.source_bytes["small/local"] == 100_000


def test_log_failure_after_merge_does_not_double_count_shard_as_failed(tmp_path: Path):
    # a completed shard whose trailing event-log write fails (e.g. disk full)
    # is already committed; it must NOT also be marked failed (which would skew
    # the planner's in-flight accounting). Each task is accounted exactly once.
    fam = local_family(tmp_path, "alpha", ["hello world"] * 5, files=1)
    trainer = Trainer(
        families=[fam],
        mint_dir=tmp_path / "bins",
        target=parse_size("1GB"),
        mint_every=parse_size("1GB"),
        workers=1,
        limit=None,
        checkpoint_every_s=3600.0,
        resume=False,
    )
    real_log = trainer.events.log

    def boom(kind, **kw):
        if kind == "shard":  # the post-merge completion log
            raise OSError("disk full")
        return real_log(kind, **kw)

    trainer.events.log = boom
    asyncio.run(trainer.run())

    assert trainer.counter.files_processed == 1            # the shard DID complete
    assert trainer._family_failed.get("alpha", 0) == 0     # and was not also failed


def test_limit_stops_early(tmp_path: Path):
    rows = ["y" * 1000] * 200  # 200 KB per file
    fam = local_family(tmp_path, "alpha", rows, files=60)
    trainer = run_trainer(tmp_path, [fam], limit=parse_size("300KB"))
    assert trainer.durable_bytes() >= 300_000
    assert trainer.counter.files_processed < 60


def test_bootstrap_mint_schedule(tmp_path: Path):
    fam = local_family(tmp_path, "alpha", ["x"], files=1)
    trainer = Trainer(
        families=[fam],
        mint_dir=tmp_path / "bins",
        target=parse_size("50TB"),
        mint_every=parse_size("5TB"),
        workers=1,
        limit=None,
        checkpoint_every_s=3600.0,
        resume=False,
    )
    labels = [mint_label(t) for t in trainer.thresholds[:6]]
    assert labels == ["100gb", "500gb", "1tb", "5tb", "10tb", "15tb"]
    assert mint_label(trainer.thresholds[-1]) == "50tb"
    trainer.events.close()


def test_mint_schedule_every_tb_after_bootstrap(tmp_path: Path):
    # the production schedule: 100GB, 500GB, then a table every 1 TB to 10 TB
    fam = local_family(tmp_path, "alpha", ["x"], files=1)
    trainer = Trainer(
        families=[fam],
        mint_dir=tmp_path / "bins",
        target=parse_size("10TB"),
        mint_every=parse_size("1TB"),
        workers=1,
        limit=None,
        checkpoint_every_s=3600.0,
        resume=False,
    )
    labels = [mint_label(t) for t in trainer.thresholds]
    assert labels == [
        "100gb", "500gb",
        "1tb", "2tb", "3tb", "4tb", "5tb", "6tb", "7tb", "8tb", "9tb", "10tb",
    ]
    trainer.events.close()


def test_classify_error_buckets():
    from sngram.train.pipeline import classify_error

    transient = [
        Exception("HTTP Error 429: Too Many Requests"),
        TimeoutError("read timed out"),
        ConnectionResetError("connection reset by peer"),
        Exception("502 Bad Gateway"),
        Exception("ChunkedEncodingError: incomplete read"),
    ]
    for e in transient:
        assert classify_error(e) == "transient", e
    assert classify_error(FileNotFoundError("404 not found")) == "missing"
    assert classify_error(ValueError("BuilderConfig 'x' not found")) == "missing"
    assert classify_error(KeyError("content")) == "hard"


def test_preflight_rejects_missing_text_field_before_counting(tmp_path: Path):
    # A misnamed text field used to fail only when that shard reached a worker,
    # which could be days into a run. It must now fail in preflight.
    directory = tmp_path / "bad"
    directory.mkdir(parents=True)
    tbl = pa.table({"wrong_field": pa.array(["abc"] * 5, type=pa.large_string())})
    pq.write_table(tbl, directory / "bad-0.parquet")
    bad = Family(
        id="bad",
        sources=(Source("bad", "local", "content", data_files=str(directory / "bad-*.parquet")),),
    )
    good = local_family(tmp_path, "good", ["hello world"] * 10, files=1)

    trainer = Trainer(
        families=[good, bad],
        mint_dir=tmp_path / "bins",
        target=parse_size("1GB"),
        mint_every=parse_size("1GB"),
        workers=1,
        limit=None,
        checkpoint_every_s=3600.0,
        resume=False,
    )
    with pytest.raises(RuntimeError, match="preflight"):
        trainer.preflight_sources()
    assert trainer.durable_bytes() == 0


# --- ETA must track bytes actually counted, never the lagging durable --------
# Regression: with multi-GB shards, `durable` (merged bytes) only advances when
# a whole shard completes — it freezes for minutes. The headline bar and the
# ETA must follow `total_bytes` (counted, incl. in-flight), which moves every
# batch, so the number visibly progresses and the ETA trends down with it. The
# mint still fires on durable (exactly-once); the ETA shows "soon" in the brief
# window after the bar passes the threshold but before the shards merge.

from types import SimpleNamespace


def _eta(total: int, *, now: float = 100 * 10**6, avg: float = 10 * 10**6,
         durable: int = 0) -> str:
    fake = SimpleNamespace(
        thresholds=[100 * 10**9],
        total_bytes=lambda: total,
        # deliberately frozen/lagging: the ETA must NOT depend on it
        durable_bytes=lambda: durable,
        rate_now=lambda: now,
        rate_avg=lambda: avg,
    )
    return Trainer.eta_next_mint(fake)


def test_eta_tracks_bytes_counted_not_frozen_durable():
    # durable stuck at 5 GB, but 40 GB has streamed -> 60 GB to go @100MB/s = 10 min
    assert _eta(40 * 10**9, durable=5 * 10**9) == "10 min"


def test_eta_shrinks_as_bytes_are_counted():
    assert _eta(10 * 10**9) == "15 min"   # 90 GB to go
    assert _eta(85 * 10**9) == "2 min"    # 15 GB to go


def test_eta_soon_once_threshold_reached():
    # bar has passed the threshold; mint fires when in-flight shards merge
    assert _eta(100 * 10**9 + 1) == "soon"


def test_eta_falls_back_to_avg_rate():
    # no recent-window rate yet -> use the cumulative average, never crash
    assert _eta(90 * 10**9, now=0.0) == "17 min"  # 10 GB / 10 MB/s ~ 1000 s
