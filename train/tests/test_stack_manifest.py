import threading
from pathlib import Path

import pyarrow as pa
import pytest

from sngram_train.catalog import Catalog, FormatSpec, build_catalog
from sngram_train.errors import ConfigurationError
from sngram_train.manifest import open_manifest
from sngram_train.scanning import skip_reason
from sngram_train.stack import build_stack_manifest, extend_manifest
from sngram_train.stackrows import HuggingFaceRows, _sampled_rows, _validate_columns


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


class Recorder:
    def __init__(self):
        self.finished_configs = []
        self.started_configs = []

    def started(self, config):
        self.started_configs.append(config)

    def scanned(self, config, rows, accepted_bytes):
        pass

    def finished(self, config, accepted, effective, seconds):
        self.finished_configs.append((config, accepted, effective, seconds))


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
    recorder = Recorder()

    roster_hash = build_stack_manifest(path, catalog, rows, recorder)
    manifest = open_manifest(path, roster_hash)

    assert rows.calls == ["Text"]
    assert recorder.finished_configs[0][:3] == ("Text", 3, 60_000)
    assert recorder.started_configs == ["Text"]
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
            "license_type": ["permissive"],
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
        FormatSpec("a", "code", "a", 1_000_000),
        FormatSpec("b", "code", "b", 1_000_000),
        FormatSpec("c", "code", "c", 1_000_000),
    )
    catalog = Catalog(formats, ("a", "b", "c"))
    values = {
        "a": [row("a-0", path="/a", extension="a", length_bytes=10_000)],
        "b": [
            row(f"b-{index}", path="/b", extension="b", length_bytes=20_000)
            for index in range(100)
        ],
        "c": [
            row(f"c-{index}", path="/c", extension="c", length_bytes=20_000)
            for index in range(100)
        ],
    }
    rows = FakeRows(values)
    path = tmp_path / "manifest.sqlite3"

    roster_hash = build_stack_manifest(
        path, catalog, rows, target=400_000, area_weights={"code": 1}
    )
    manifest = open_manifest(path, roster_hash)

    assert rows.calls.count("a") == 1
    assert rows.calls.count("b") >= 2
    assert manifest.capacity("a") == 10_000
    assert manifest.capacity("b") == manifest.capacity("c")
    assert manifest.capacity("b") >= 195_000
    assert sum(manifest.capacity(item.id) for item in formats) < 3_000_000
    assert manifest.effective_target == 400_000


def test_infeasible_target_clamps_to_corpus_supply(tmp_path: Path):
    formats = (
        FormatSpec("a", "code", "a", 1_000_000),
        FormatSpec("b", "code", "b", 1_000_000),
    )
    catalog = Catalog(formats, ("a", "b"))
    values = {
        "a": [row("a-0", path="/a", extension="a", length_bytes=30_000)],
        "b": [row("b-0", path="/b", extension="b", length_bytes=50_000)],
    }
    path = tmp_path / "manifest.sqlite3"

    roster_hash = build_stack_manifest(
        path, catalog, FakeRows(values), target=1_000_000, area_weights={"code": 1}
    )
    manifest = open_manifest(path, roster_hash)

    assert manifest.effective_target == 80_000
    assert manifest.capacity("a") == 30_000
    assert manifest.capacity("b") == 50_000


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
    values = []
    for index in range(4):
        values.append(row(f"docs-{index}", path=f"/notes-{index}.txt", extension="txt"))
        values.append(row(f"config-{index}", path=f"/cfg/app-{index}.json", extension="json"))
        values.append(row(f"data-{index}", path=f"/data/items-{index}.csv", extension="csv"))
    values.extend(
        row(f"extra-{index}", path="/more.txt", extension="txt") for index in range(100)
    )
    rows = FakeRows({"Text": values})
    path = tmp_path / "manifest.sqlite3"

    roster_hash = build_stack_manifest(
        path,
        catalog,
        rows,
        target=60_000,
        area_weights={area: 1 for area in {item.area for item in catalog.formats}},
    )
    manifest = open_manifest(path, roster_hash)

    assert manifest.capacity("docs-prose-markup/Text") == 60_000
    assert manifest.capacity("config-build-infra/Text") == 60_000
    assert manifest.capacity("data-query-schema/Text") == 60_000
    assert rows.cursors == [("Text", (0, 0))]


def test_extension_grows_one_starved_format_from_its_cursor(tmp_path: Path):
    formats = (
        FormatSpec("a", "code", "a", 1_000_000),
        FormatSpec("b", "code", "b", 1_000_000),
    )
    catalog = Catalog(formats, ("a", "b"))
    values = {
        "a": [
            row(f"a-{index}", path="/a", extension="a", length_bytes=20_000)
            for index in range(50)
        ],
        "b": [
            row(f"b-{index}", path="/b", extension="b", length_bytes=20_000)
            for index in range(50)
        ],
    }
    path = tmp_path / "manifest.sqlite3"
    rows = FakeRows(values)
    roster_hash = build_stack_manifest(
        path, catalog, rows, target=200_000, area_weights={"code": 1}
    )
    before = open_manifest(path, roster_hash)

    extend_manifest(path, catalog, rows, roster_hash, "a", before.capacity("a") + 100_000)
    after = open_manifest(path, roster_hash)

    assert after.capacity("a") >= before.capacity("a") + 100_000
    assert after.capacity("b") == before.capacity("b")
    assert after.candidates("a") > before.candidates("a")


def test_excluded_configs_are_dropped_from_the_catalog():
    catalog = build_catalog(["Python", "Jupyter_Notebook", "Markdown"])

    assert not any("Jupyter" in item.id for item in catalog.formats)
    assert "Jupyter_Notebook" not in catalog.configs


def test_per_config_file_cap_rejects_bloated_config_files():
    big = row("big", path="/a.json", extension="json", length_bytes=200 * 1024)

    assert skip_reason(big, "config-build-infra", 128 * 1024) == "oversize"
    assert skip_reason(big, "config-build-infra", None) is None


def test_excluded_extension_drops_ocr_layout_dumps():
    scan = row("ocr", path="/page.hocr", extension="hocr", language="XML")
    kept = row("ok", path="/data.xml", extension="xml", language="XML")

    assert skip_reason(scan, "config-build-infra") == "excluded_extension"
    assert skip_reason(kept, "config-build-infra") is None


def test_manifest_captures_extension_and_license(tmp_path: Path):
    import sqlite3

    catalog = build_catalog(["Python"])
    py = dict(
        row("p", path="/m.py", extension="py", language="Python"),
        license_type="mit",
    )
    rows = FakeRows({"Python": [py]})
    path = tmp_path / "m.sqlite3"

    build_stack_manifest(path, catalog, rows, None)

    with sqlite3.connect(path) as connection:
        extension, license_type = connection.execute(
            "SELECT extension, license FROM candidates LIMIT 1"
        ).fetchone()
    assert extension == "py"
    assert license_type == "mit"


def test_training_read_path_ignores_the_new_metadata_columns(tmp_path: Path):
    catalog = build_catalog(["Python"])
    py = dict(row("p", path="/m.py", extension="py", language="Python"), license_type="mit")
    path = tmp_path / "m.sqlite3"

    roster = build_stack_manifest(path, catalog, FakeRows({"Python": [py]}), None)
    manifest = open_manifest(path, roster)
    item = manifest.read("core-programming/Python", 0, 10).items[0]

    assert item.blob_id == "p"
    assert item.length == 20_000


def test_adaptive_inventory_tracks_the_target_area_weights(tmp_path: Path):
    from sngram_train.config import STACK_V2_BUCKET_CAPS

    def many(prefix, ext, lang):
        return [
            row(f"{prefix}{i}", path=f"/f.{ext}", extension=ext, language=lang)
            for i in range(4000)
        ]

    configs = {
        "Python": many("py", "py", "Python"),
        "Markdown": many("md", "md", "Markdown"),
        "JSON": many("js", "json", "JSON"),
        "HTML": many("ht", "html", "HTML"),
        "SQL": many("sq", "sql", "SQL"),
        "1C_Enterprise": many("tc", "1c", "1C Enterprise"),
        "Jupyter_Notebook": many("nb", "ipynb", "Jupyter Notebook"),
    }
    catalog = build_catalog(list(configs))
    path = tmp_path / "m.sqlite3"
    roster = build_stack_manifest(
        path, catalog, FakeRows(configs), None,
        target=20_000_000, area_weights=STACK_V2_BUCKET_CAPS, workers=1,
    )
    manifest = open_manifest(path, roster)

    areas: dict[str, int] = {}
    for fmt in catalog.formats:
        areas[fmt.area] = areas.get(fmt.area, 0) + manifest.capacity(fmt.id)
    total = sum(areas.values())
    share = {a: v / total for a, v in areas.items()}

    assert not any("Jupyter" in fmt.id for fmt in catalog.formats)
    assert abs(share["core-programming"] - 0.380) < 0.02
    assert abs(share["config-build-infra"] - 0.197) < 0.02
    assert abs(share["docs-prose-markup"] - 0.140) < 0.02
    assert abs(share["web-ui-templates"] - 0.137) < 0.02
    assert abs(share["data-query-schema"] - 0.116) < 0.02
    assert abs(share["long-tail"] - 0.030) < 0.02
