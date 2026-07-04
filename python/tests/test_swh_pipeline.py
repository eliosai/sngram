"""Stack v2 / Software Heritage training path over local fixtures."""

from __future__ import annotations

import asyncio
import gzip
import json
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

from sngram.train.config import Family, Source, STACK_V2_REQUIRED_COLUMNS
from sngram.train.pipeline import Trainer
from sngram.train.units import parse_size


def _write_content(directory: Path, blob_id: str, content: bytes) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    with gzip.open(directory / blob_id, "wb") as fh:
        fh.write(content)


def _write_metadata(directory: Path, rows: list[dict[str, object]]) -> str:
    directory.mkdir(parents=True, exist_ok=True)
    columns = {
        "blob_id": pa.array([r.get("blob_id") for r in rows], type=pa.string()),
        "content_id": pa.array([r.get("content_id") for r in rows], type=pa.string()),
        "src_encoding": pa.array([r.get("src_encoding") for r in rows], type=pa.string()),
        "language": pa.array([r.get("language") for r in rows], type=pa.string()),
        "path": pa.array([r.get("path") for r in rows], type=pa.string()),
        "extension": pa.array([r.get("extension") for r in rows], type=pa.string()),
        "is_vendor": pa.array([bool(r.get("is_vendor")) for r in rows], type=pa.bool_()),
        "is_generated": pa.array([bool(r.get("is_generated")) for r in rows], type=pa.bool_()),
        "length_bytes": pa.array([int(r.get("length_bytes") or 0) for r in rows], type=pa.int64()),
    }
    pq.write_table(pa.table(columns), directory / "metadata-0.parquet")
    return str(directory / "metadata-*.parquet")


def _row(
    blob_id: str,
    content: bytes,
    *,
    language: str = "Python",
    extension: str = "py",
    path: str = "/src/main.py",
    src_encoding: str = "utf-8",
    is_vendor: bool = False,
    is_generated: bool = False,
) -> dict[str, object]:
    return {
        "blob_id": blob_id,
        "content_id": f"content-{blob_id}",
        "src_encoding": src_encoding,
        "language": language,
        "path": path,
        "extension": extension,
        "is_vendor": is_vendor,
        "is_generated": is_generated,
        "length_bytes": len(content),
    }


def _swh_family(
    metadata_glob: str,
    content_dir: Path,
    bucket: str,
    cap_bytes: int,
) -> Family:
    family_id = f"stack-{bucket}"
    return Family(
        id=family_id,
        weight=cap_bytes,
        cap_bytes=cap_bytes,
        bucket=bucket,
        sources=(
            Source(
                family_id,
                "bigcode/the-stack-v2-dedup",
                "blob_id",
                config=bucket,
                cap_bytes=cap_bytes,
                format="swh",
                data_files=metadata_glob,
                content_prefix=f"file://{content_dir}/",
                metadata_fields=STACK_V2_REQUIRED_COLUMNS,
            ),
        ),
    )


def _run(tmp_path: Path, family: Family, target: int) -> Trainer:
    trainer = Trainer(
        families=[family],
        mint_dir=tmp_path / "bins",
        target=target,
        mint_every=parse_size("1GB"),
        workers=1,
        limit=None,
        checkpoint_every_s=3600.0,
        resume=False,
    )
    asyncio.run(trainer.run())
    return trainer


def _events(tmp_path: Path) -> list[dict[str, object]]:
    return [
        json.loads(line)
        for line in (tmp_path / "bins" / "train-events.jsonl").read_text().splitlines()
    ]


def test_swh_source_counts_matching_bucket_content_and_logs_telemetry(tmp_path: Path):
    content_dir = tmp_path / "content"
    py = b"def add(a, b):\n    return a + b\n"
    md = b"# Guide\nThis row belongs to docs.\n"
    generated = b"print('generated')\n"
    _write_content(content_dir, "py-blob", py)
    _write_content(content_dir, "md-blob", md)
    _write_content(content_dir, "generated-blob", generated)
    metadata = _write_metadata(
        tmp_path / "metadata",
        [
            _row("py-blob", py),
            _row(
                "md-blob", md,
                language="Markdown", extension="md", path="/README.md",
            ),
            _row("generated-blob", generated, is_generated=True),
        ],
    )
    family = _swh_family(metadata, content_dir, "core-programming", len(py))

    trainer = _run(tmp_path, family, target=len(py))

    assert trainer.durable_bytes() == len(py)
    events = _events(tmp_path)
    manifest_start = next(e for e in events if e["kind"] == "swh_manifest_start")
    assert manifest_start["content_prefix"] == f"file://{content_dir}/"
    assert "revision" in manifest_start
    assert any(e["kind"] == "swh_manifest_done" for e in events)
    batch = next(e for e in events if e["kind"] == "s3_batch")
    assert batch["accepted_objects"] == 1
    assert batch["skipped_bucket"] == 1
    assert batch["skipped_generated"] == 1
    progress = next(e for e in events if e["kind"] == "swh_bucket_progress")
    assert progress["bucket"] == "core-programming"
    assert progress["accepted_bytes"] == len(py)


def test_swh_source_fills_cap_without_overcounting_content(tmp_path: Path):
    content_dir = tmp_path / "content"
    content = ("abc123" * 100).encode()
    _write_content(content_dir, "large-blob", content)
    metadata = _write_metadata(
        tmp_path / "metadata",
        [_row("large-blob", content)],
    )
    family = _swh_family(metadata, content_dir, "core-programming", 37)

    trainer = _run(tmp_path, family, target=37)

    assert trainer.durable_bytes() == 37
    assert trainer.state.family_bytes["stack-core-programming"] == 37
    assert trainer.state.source_bytes["stack-core-programming/core-programming"] == 37


def test_swh_object_errors_are_logged_and_do_not_stop_the_shard(tmp_path: Path):
    content_dir = tmp_path / "content"
    bad_ascii = b"\xff\xfe"
    good = b"def ok():\n    return True\n"
    _write_content(content_dir, "bad-decode", bad_ascii)
    _write_content(content_dir, "good-blob", good)
    metadata = _write_metadata(
        tmp_path / "metadata",
        [
            _row("missing-blob", b"not present"),
            _row("bad-decode", bad_ascii, src_encoding="ascii"),
            _row("good-blob", good),
        ],
    )
    family = _swh_family(metadata, content_dir, "core-programming", len(good))

    trainer = _run(tmp_path, family, target=len(good))

    assert trainer.durable_bytes() == len(good)
    errors = [e for e in _events(tmp_path) if e["kind"] == "s3_object_error"]
    assert {e["stage"] for e in errors} == {"fetch", "decode"}
    batch = next(e for e in _events(tmp_path) if e["kind"] == "s3_batch")
    assert batch["fetch_errors"] == 1
    assert batch["decode_errors"] == 1
    assert batch["accepted_objects"] == 1


def test_swh_preflight_requires_stack_v2_metadata_columns(tmp_path: Path):
    metadata_dir = tmp_path / "metadata"
    metadata_dir.mkdir(parents=True)
    pq.write_table(
        pa.table(
            {
                "blob_id": pa.array(["blob"], type=pa.string()),
                "content_id": pa.array(["content"], type=pa.string()),
                "language": pa.array(["Python"], type=pa.string()),
                "path": pa.array(["/src/main.py"], type=pa.string()),
                "extension": pa.array(["py"], type=pa.string()),
                "is_vendor": pa.array([False], type=pa.bool_()),
                "is_generated": pa.array([False], type=pa.bool_()),
                "length_bytes": pa.array([10], type=pa.int64()),
            }
        ),
        metadata_dir / "metadata-0.parquet",
    )
    family = _swh_family(
        str(metadata_dir / "metadata-*.parquet"),
        tmp_path / "content",
        "core-programming",
        10,
    )
    trainer = Trainer(
        families=[family],
        mint_dir=tmp_path / "bins",
        target=10,
        mint_every=parse_size("1GB"),
        workers=1,
        limit=None,
        checkpoint_every_s=3600.0,
        resume=False,
    )

    try:
        trainer.preflight_sources()
    except RuntimeError as e:
        assert "src_encoding" in str(e)
    else:  # pragma: no cover - failure path
        raise AssertionError("preflight should reject missing src_encoding")
