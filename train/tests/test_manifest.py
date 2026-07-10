import sqlite3
from pathlib import Path

import pytest

from sngram_train.manifest import Candidate, ManifestBuilder, open_manifest


def test_manifest_round_trip_and_cursor_are_stable(tmp_path: Path):
    path = tmp_path / "manifest.sqlite3"
    with ManifestBuilder(path, revision="abc", roster_hash="roster") as builder:
        builder.add(Candidate("core/Python", "one", "utf-8", 100, 4))
        builder.add(Candidate("core/Python", "two", "utf-8", 200, 1))
        builder.add(Candidate("docs/Markdown", "three", "utf-8", 50, 2))

    manifest = open_manifest(path, roster_hash="roster")
    first = manifest.read("core/Python", cursor=0, limit=1)
    second = manifest.read("core/Python", cursor=first.cursor, limit=10)

    assert manifest.revision == "abc"
    assert manifest.capacity("core/Python") == 600
    assert [item.blob_id for item in first.items] == ["one"]
    assert [item.blob_id for item in second.items] == ["two"]
    assert second.exhausted is True


def test_manifest_compacts_hex_blob_ids_and_encodings(tmp_path: Path):
    path = tmp_path / "manifest.sqlite3"
    blob_id = "0123456789abcdef0123456789abcdef01234567"
    with ManifestBuilder(path, revision="abc", roster_hash="roster") as builder:
        builder.add(Candidate("core/Python", blob_id, "UTF-8", 100, 1))

    manifest = open_manifest(path, roster_hash="roster")
    item = manifest.read("core/Python", cursor=0, limit=1).items[0]
    with sqlite3.connect(path) as connection:
        stored = connection.execute(
            "SELECT typeof(blob_id), length(blob_id), typeof(encoding_key) "
            "FROM candidates"
        ).fetchone()

    assert item.blob_id == blob_id
    assert item.encoding == "UTF-8"
    assert stored == ("blob", 21, "integer")


def test_manifest_rejects_a_different_roster(tmp_path: Path):
    path = tmp_path / "manifest.sqlite3"
    with ManifestBuilder(path, revision="abc", roster_hash="old"):
        pass

    try:
        open_manifest(path, roster_hash="new")
    except RuntimeError as error:
        assert "roster" in str(error)
    else:
        raise AssertionError("manifest should reject a different roster")


def test_manifest_rejects_a_concurrent_builder(tmp_path: Path):
    path = tmp_path / "manifest.sqlite3"
    with ManifestBuilder(path, revision="abc", roster_hash="roster"):
        with pytest.raises(RuntimeError, match="another process"):
            with ManifestBuilder(path, revision="abc", roster_hash="roster"):
                pass


def test_manifest_resumes_completed_configs_and_rolls_back_partial_one(tmp_path: Path):
    path = tmp_path / "manifest.sqlite3"
    with pytest.raises(RuntimeError, match="interrupt"):
        with ManifestBuilder(path, revision="abc", roster_hash="roster") as builder:
            builder.add(Candidate("core/Python", "python", "utf-8", 100, 1))
            builder.finish_config("Python", cursor=(3, 7))
            builder.add(Candidate("core/Rust", "partial", "utf-8", 100, 1))
            raise RuntimeError("interrupt")

    with ManifestBuilder(path, revision="abc", roster_hash="roster") as builder:
        assert builder.is_complete("Python")
        assert builder.cursor("Python") == (3, 7)
        builder.add(Candidate("core/Rust", "rust", "utf-8", 100, 1))
        builder.finish_config("Rust")

    manifest = open_manifest(path, "roster")
    assert [item.blob_id for item in manifest.read("core/Python", 0, 10).items] == [
        "python"
    ]
    assert [item.blob_id for item in manifest.read("core/Rust", 0, 10).items] == ["rust"]
