from pathlib import Path

from sngram_train import dataset
from sngram_train.manifest import (
    Candidate,
    ManifestBuilder,
    open_manifest,
    read_metadata,
)

HEX = "a" * 39


def build_small(path: Path) -> None:
    with ManifestBuilder(path, revision="rev", roster_hash="legacy") as builder:
        for index in range(5):
            builder.add(
                Candidate("core-programming/Python", f"{HEX}{index}", "utf-8", 100 + index, 1)
            )
        for index in range(3):
            builder.add(
                Candidate("docs-prose-markup/Markdown", f"doc-{index}", "latin-1", 200, 2)
            )
        builder.set_exhausted("docs-prose-markup/Markdown")
        builder.set_built_target(10_000)
        builder.set_effective_target(4_000)


def test_export_writes_a_sharded_parquet_dataset(tmp_path: Path):
    build_small(tmp_path / "src.sqlite3")

    out = tmp_path / "ds"
    sidecar = dataset.export_dataset(tmp_path / "src.sqlite3", out)

    assert (out / dataset.MANIFEST_META).exists()
    assert list((out / dataset.DATA_DIR).glob("*.parquet"))
    assert sidecar["built_target"] == 10_000
    assert sidecar["effective_target"] == 4_000
    assert {"id": "docs-prose-markup/Markdown", "exhausted": True} in sidecar["formats"]


def test_round_trip_reproduces_candidates_and_flags(tmp_path: Path):
    build_small(tmp_path / "src.sqlite3")
    out = tmp_path / "ds"
    dataset.export_dataset(tmp_path / "src.sqlite3", out)

    dest = tmp_path / "dest.sqlite3"
    roster = dataset.import_dataset(out, dest)
    manifest = open_manifest(dest, roster)

    python = manifest.read("core-programming/Python", 0, 10)
    assert [item.blob_id for item in python.items] == [f"{HEX}{i}" for i in range(5)]
    assert [item.encoding for item in python.items] == ["utf-8"] * 5
    assert manifest.capacity("core-programming/Python") == sum(100 + i for i in range(5))
    markdown = manifest.read("docs-prose-markup/Markdown", 0, 10)
    assert [item.blob_id for item in markdown.items] == [f"doc-{i}" for i in range(3)]
    assert manifest.capacity("docs-prose-markup/Markdown") == 200 * 2 * 3
    assert manifest.exhausted("docs-prose-markup/Markdown") is True
    assert manifest.exhausted("core-programming/Python") is False
    assert manifest.effective_target == 4_000
    assert read_metadata(dest)["built_target"] == "10000"


def test_import_is_deterministic(tmp_path: Path):
    build_small(tmp_path / "src.sqlite3")
    out = tmp_path / "ds"
    dataset.export_dataset(tmp_path / "src.sqlite3", out)

    first = dataset.import_dataset(out, tmp_path / "one.sqlite3")
    second = dataset.import_dataset(out, tmp_path / "two.sqlite3")

    assert first == second
    left = open_manifest(tmp_path / "one.sqlite3", first)
    right = open_manifest(tmp_path / "two.sqlite3", second)
    both = ("core-programming/Python", "docs-prose-markup/Markdown")
    for format_id in both:
        a = [item.blob_id for item in left.read(format_id, 0, 10).items]
        b = [item.blob_id for item in right.read(format_id, 0, 10).items]
        assert a == b


def test_small_dataset_is_a_single_shard(tmp_path: Path):
    build_small(tmp_path / "src.sqlite3")
    out = tmp_path / "ds"
    dataset.export_dataset(tmp_path / "src.sqlite3", out)

    shards = sorted((out / dataset.DATA_DIR).glob("*.parquet"))
    assert [path.name for path in shards] == ["train-00000-of-00001.parquet"]
