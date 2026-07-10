import threading
from pathlib import Path

import pyarrow as pa
import pytest

from sngram_train.catalog import Catalog, FormatSpec, build_catalog
from sngram_train.errors import ConfigurationError
from sngram_train.manifest import open_manifest
from sngram_train.stack import (
    HuggingFaceRows,
    _sampled_rows,
    _validate_columns,
    build_stack_manifest,
    skip_reason,
)


class FakeRows:
    revision = "revision-1"

    def __init__(self, rows):
        self.rows = rows
        self.calls = []
        self.cursors = []

    def iter_rows(self, config, cursor=(0, 0)):
        self.calls.append(config)
        self.cursors.append((config, cursor))
        for index, value in enumerate(self.rows[config][cursor[1] :], cursor[1]):
            item = dict(value)
            item["_source_cursor"] = (0, index + 1)
            yield item


def row(name: str, *, path: str, extension: str, **changes):
    value = {
        "blob_id": name,
        "content_id": name,
        "src_encoding": "utf-8",
        "language": "Text",
        "path": path,
        "extension": extension,
        "is_vendor": False,
        "is_generated": False,
        "length_bytes": 20_000,
        "_sample_weight": 1,
    }
    value.update(changes)
    return value


def test_builder_scans_text_once_and_routes_each_row(tmp_path: Path):
    catalog = build_catalog(["Text"])
    rows = FakeRows(
        {
            "Text": [
                row("docs", path="/README.txt", extension="txt"),
                row("config", path="/cfg/app.json", extension="json"),
                row("data", path="/data/items.csv", extension="csv"),
            ]
        }
    )
    path = tmp_path / "manifest.sqlite3"
    updates = []

    roster_hash = build_stack_manifest(path, catalog, rows, updates.append)
    manifest = open_manifest(path, roster_hash)

    assert rows.calls == ["Text"]
    assert updates[0][:3] == ("Text", 3, 60_000)
    assert updates[0][3] >= 0
    assert manifest.read("docs-prose-markup/Text", 0, 10).items[0].blob_id == "docs"
    assert manifest.read("config-build-infra/Text", 0, 10).items[0].blob_id == "config"
    assert manifest.read("data-query-schema/Text", 0, 10).items[0].blob_id == "data"


def test_invalid_rows_are_rejected_without_a_small_file_cutoff():
    valid = row("small", path="/x.txt", extension="txt", length_bytes=1)

    assert skip_reason(valid, "docs-prose-markup") is None
    assert skip_reason(dict(valid, is_vendor=True), "docs-prose-markup") == "vendor"
    assert skip_reason(dict(valid, is_generated=True), "docs-prose-markup") == "generated"
    assert skip_reason(dict(valid, length_bytes=0), "docs-prose-markup") == "empty"
    assert skip_reason(dict(valid, length_bytes=4 * 1024 * 1024 + 1), "docs-prose-markup") == (
        "oversize"
    )


def test_arrow_batch_samples_small_rows_before_materializing_them():
    size = 4096
    count = 4096
    batch = pa.record_batch(
        {
            "blob_id": [f"blob-{index}" for index in range(count)],
            "content_id": [f"content-{index}" for index in range(count)],
            "src_encoding": ["utf-8"] * count,
            "language": ["Python"] * count,
            "path": ["/src/main.py"] * count,
            "extension": ["py"] * count,
            "is_vendor": [False] * count,
            "is_generated": [False] * count,
            "length_bytes": [size] * count,
        }
    )

    rows = list(_sampled_rows(batch, seed=7, offset=0))

    assert 900 <= len(rows) <= 1150
    assert {item["_sample_weight"] for item in rows} == {4}


def test_stack_inventory_resumes_at_the_first_incomplete_config(tmp_path: Path):
    catalog = build_catalog(["Python", "Text"])
    python = row("python", path="/main.py", extension="py", language="Python")
    text = row("text", path="/README.txt", extension="txt")

    class FailingRows(FakeRows):
        def iter_rows(self, config, cursor=(0, 0)):
            self.calls.append(config)
            self.cursors.append((config, cursor))
            if config == "Text":
                raise RuntimeError("interrupt")
            for index, value in enumerate(self.rows[config][cursor[1] :], cursor[1]):
                item = dict(value)
                item["_source_cursor"] = (0, index + 1)
                yield item

    path = tmp_path / "manifest.sqlite3"
    with pytest.raises(RuntimeError, match="interrupt"):
        build_stack_manifest(path, catalog, FailingRows({"Python": [python], "Text": []}))

    resumed = FakeRows({"Python": [python], "Text": [text]})
    roster_hash = build_stack_manifest(path, catalog, resumed)
    manifest = open_manifest(path, roster_hash)

    assert resumed.calls == ["Text"]
    assert manifest.capacity("core-programming/Python") == 20_000
    assert manifest.capacity("docs-prose-markup/Text") == 20_000


def test_stack_file_tree_is_resolved_once_for_all_configs():
    class FakeFileSystem:
        calls = []

        def glob(self, pattern):
            self.calls.append(pattern)
            prefix = "datasets/bigcode/the-stack-v2-dedup@rev/data"
            return [
                f"{prefix}/Rust/train-1.parquet",
                f"{prefix}/Python/train-2.parquet",
                f"{prefix}/Python/train-1.parquet",
            ]

    rows = HuggingFaceRows.__new__(HuggingFaceRows)
    rows.revision = "rev"
    rows._fs = FakeFileSystem()
    rows._files = None

    assert rows.configs() == ["Python", "Rust"]
    assert len(rows._shards("Python")) == 2
    assert len(rows._fs.calls) == 1


def test_stack_rows_use_bounded_readahead_for_remote_parquet(tmp_path: Path):
    parquet_path = tmp_path / "rows.parquet"
    batch = pa.table(
        {
            "blob_id": ["blob"],
            "content_id": ["content"],
            "src_encoding": ["utf-8"],
            "language": ["Rust"],
            "path": ["/src/main.rs"],
            "extension": ["rs"],
            "is_vendor": [False],
            "is_generated": [False],
            "length_bytes": [20_000],
        }
    )
    import pyarrow.parquet as pq

    pq.write_table(batch, parquet_path)

    class RemoteFileSystem:
        def open(self, _path, _mode, **options):
            if options != {"cache_type": "readahead", "block_size": 64 * 1024 * 1024}:
                raise RuntimeError(f"unbounded remote read: {options}")
            return parquet_path.open("rb")

    rows = HuggingFaceRows.__new__(HuggingFaceRows)
    rows.revision = "rev"
    rows._fs = RemoteFileSystem()
    rows._files = {"Rust": ["datasets/repo@rev/data/Rust/train-0.parquet"]}

    assert [item["blob_id"] for item in rows.iter_rows("Rust")] == ["blob"]


def test_stack_schema_failure_is_a_configuration_error():
    with pytest.raises(ConfigurationError, match="src_encoding"):
        _validate_columns(["blob_id", "content_id"])


def test_inventory_extends_live_formats_without_materializing_all_caps(tmp_path: Path):
    formats = (
        FormatSpec("a", "code", "a", 1_000),
        FormatSpec("b", "code", "b", 1_000),
        FormatSpec("c", "code", "c", 1_000),
    )
    catalog = Catalog(formats, ("a", "b", "c"))
    values = {
        "a": [row("a-0", path="/a", extension="a", length_bytes=10)],
        "b": [
            row(f"b-{index}", path="/b", extension="b", length_bytes=20)
            for index in range(10)
        ],
        "c": [
            row(f"c-{index}", path="/c", extension="c", length_bytes=20)
            for index in range(10)
        ],
    }
    rows = FakeRows(values)
    path = tmp_path / "manifest.sqlite3"

    roster_hash = build_stack_manifest(
        path, catalog, rows, target=100, area_weights={"code": 1}
    )
    manifest = open_manifest(path, roster_hash)

    assert rows.calls.count("a") == 1
    assert rows.calls.count("b") == 2
    assert rows.calls.count("c") == 2
    assert ("b", (0, 2)) in rows.cursors
    assert ("c", (0, 2)) in rows.cursors
    assert manifest.capacity("a") == 10
    assert manifest.capacity("b") == 60
    assert manifest.capacity("c") == 60
    assert sum(manifest.capacity(item.id) for item in formats) < 3_000


def test_manifest_scans_independent_configs_concurrently(tmp_path: Path):
    formats = (
        FormatSpec("a", "code", "a", 100),
        FormatSpec("b", "code", "b", 100),
    )
    catalog = Catalog(formats, ("a", "b"))

    class ConcurrentRows(FakeRows):
        def __init__(self):
            super().__init__(
                {
                    "a": [row("a", path="/a", extension="a", length_bytes=20)],
                    "b": [row("b", path="/b", extension="b", length_bytes=20)],
                }
            )
            self.barrier = threading.Barrier(2)

        def iter_rows(self, config, cursor=(0, 0)):
            self.barrier.wait(timeout=1)
            yield from super().iter_rows(config, cursor)

    path = tmp_path / "manifest.sqlite3"
    rows = ConcurrentRows()

    roster_hash = build_stack_manifest(
        path,
        catalog,
        rows,
        target=40,
        area_weights={"code": 1},
        workers=2,
    )
    manifest = open_manifest(path, roster_hash)

    assert manifest.capacity("a") == 20
    assert manifest.capacity("b") == 20


def test_target_bounded_text_inventory_stops_at_route_goals(tmp_path: Path):
    catalog = build_catalog(["Text"])
    values = [
        row("docs-0", path="/README.txt", extension="txt"),
        row("docs-1", path="/guide.md", extension="md"),
        row("config", path="/cfg/app.json", extension="json"),
        row("data", path="/data/items.csv", extension="csv"),
    ]
    values.extend(
        row(f"extra-{index}", path="/notes.txt", extension="txt")
        for index in range(100)
    )
    rows = FakeRows({"Text": values})
    path = tmp_path / "manifest.sqlite3"

    roster_hash = build_stack_manifest(
        path,
        catalog,
        rows,
        target=60_000,
        area_weights={area: 1 for area in {
            item.area for item in catalog.formats
        }},
    )
    manifest = open_manifest(path, roster_hash)

    assert manifest.capacity("docs-prose-markup/Text") == 40_000
    assert manifest.capacity("config-build-infra/Text") == 20_000
    assert manifest.capacity("data-query-schema/Text") == 20_000
    assert rows.cursors == [("Text", (0, 0))]
