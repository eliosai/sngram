import sqlite3
from pathlib import Path

import pytest

from sngram_train.manifest import ManifestWriter, open_manifest


def row(blob_id, length, weight=1, encoding="utf-8"):
    return (blob_id, encoding, length, weight, "", "")


def write_manifest(path: Path, rows_by_format, exhausted=()):
    with ManifestWriter(path, revision="abc", roster_hash="roster") as writer:
        for format_id in rows_by_format:
            writer.register(format_id, format_id in exhausted)
        for format_id, rows in rows_by_format.items():
            writer.add_rows(format_id, rows)


def test_manifest_round_trip_and_cursor_are_stable(tmp_path: Path):
    path = tmp_path / "manifest.sqlite3"
    write_manifest(
        path,
        {
            "core/Python": [row("one", 100, 4), row("two", 200)],
            "docs/Markdown": [row("three", 50, 2)],
        },
    )

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
    write_manifest(path, {"core/Python": [row(blob_id, 100, encoding="UTF-8")]})

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
    write_manifest(path, {"core/Python": [row("one", 100)]})

    with pytest.raises(RuntimeError, match="roster"):
        open_manifest(path, roster_hash="new")


def test_manifest_records_exhausted_formats_and_targets(tmp_path: Path):
    path = tmp_path / "manifest.sqlite3"
    with ManifestWriter(path, revision="abc", roster_hash="roster") as writer:
        writer.register("core/Python", exhausted=True)
        writer.register("docs/Markdown")
        writer.add_rows("core/Python", [row("one", 100)])
        writer.set_targets(1_000, 600)

    manifest = open_manifest(path, roster_hash="roster")

    assert manifest.exhausted("core/Python") is True
    assert manifest.exhausted("docs/Markdown") is False
    assert manifest.effective_target == 600


def test_failed_write_leaves_no_manifest(tmp_path: Path):
    path = tmp_path / "manifest.sqlite3"
    with pytest.raises(RuntimeError, match="boom"):
        with ManifestWriter(path, revision="abc", roster_hash="roster") as writer:
            writer.register("core/Python")
            writer.add_rows("core/Python", [row("one", 100)])
            raise RuntimeError("boom")

    assert not path.exists()
    assert not path.with_suffix(path.suffix + ".tmp").exists()


def test_manifest_reads_are_stable_across_the_read_ahead_window(tmp_path: Path):
    from sngram_train.manifest import READ_AHEAD_ROWS

    path = tmp_path / "manifest.sqlite3"
    count = READ_AHEAD_ROWS + 50
    write_manifest(
        path, {"core/Python": [row(f"blob-{index:05d}", 10) for index in range(count)]}
    )

    manifest = open_manifest(path, roster_hash="roster")
    bulk = manifest.read("core/Python", cursor=0, limit=count).items
    stepped = []
    cursor = 0
    while True:
        batch = manifest.read("core/Python", cursor, limit=7)
        stepped.extend(batch.items)
        cursor = batch.cursor
        if batch.exhausted:
            break
    rewound = manifest.read("core/Python", cursor=3, limit=7).items

    assert [item.blob_id for item in stepped] == [item.blob_id for item in bulk]
    assert [item.blob_id for item in rewound] == [item.blob_id for item in bulk[3:10]]
