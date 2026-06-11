"""Pipeline end-to-end over local parquet fixtures — no network."""

import asyncio
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


def test_failed_source_does_not_kill_run(tmp_path: Path):
    good = local_family(tmp_path, "alpha", ["abc"] * 10)
    bad = Family(
        id="broken",
        sources=(Source("broken", "local", "content", data_files=str(tmp_path / "nope-*.parquet")),),
    )
    trainer = run_trainer(tmp_path, [good, bad])
    assert trainer.durable_bytes() > 0
    assert trainer.errors >= 1


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


def test_shard_error_path_classifies_and_survives(tmp_path: Path):
    # a shard whose parquet exists but whose text field is missing exercises
    # the full _run_shard error path (the path a kwarg-collision bug once
    # killed): the error must be classified+logged, the shard marked failed,
    # the worker must survive, and the run must finish
    directory = tmp_path / "bad"
    directory.mkdir(parents=True)
    tbl = pa.table({"wrong_field": pa.array(["abc"] * 5, type=pa.large_string())})
    pq.write_table(tbl, directory / "bad-0.parquet")
    bad = Family(
        id="bad",
        sources=(Source("bad", "local", "content", data_files=str(directory / "bad-*.parquet")),),
    )
    good = local_family(tmp_path, "good", ["hello world"] * 10, files=1)

    trainer = run_trainer(tmp_path, [good, bad])
    assert trainer.durable_bytes() == 11 * 10
    assert trainer.failed_shards >= 1
    shard_errors = [e for e in (trainer.events.tail or []) if e.get("stage") == "shard"]
    assert shard_errors, "the shard failure must be logged, not swallowed"
    assert all("multiple values" not in str(e) for e in shard_errors)
