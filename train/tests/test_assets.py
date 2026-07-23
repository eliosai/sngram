import gzip
import json
from pathlib import Path

import pytest

from sngram_train import assets
from sngram_train.catalog import build_catalog
from sngram_train.errors import ConfigurationError
from sngram_train.manifest import open_manifest

REVISION = "rev-1"

ROWS = [
    {"group": "code", "language": "Python", "extension": "py", "license": "permissive",
     "blob_id": "b0", "encoding": "UTF-8", "length": 100, "weight": 4},
    {"group": "code", "language": "Python", "extension": "py", "license": "no_license",
     "blob_id": "b1", "encoding": "UTF-8", "length": 200, "weight": 1},
    {"group": "docs", "language": "Markdown", "extension": "md", "license": "permissive",
     "blob_id": "b2", "encoding": "UTF-8", "length": 50, "weight": 2},
]


def catalog_for(rows):
    return build_catalog(sorted({item["language"] for item in rows}))


def sidecar_for(rows, catalog):
    counts: dict[str, int] = {}
    for item in rows:
        format_id = f"{assets._AREA_BY_LABEL[item['group']]}/{item['language']}"
        counts[format_id] = counts.get(format_id, 0) + 1
    return {
        "revision": REVISION,
        "roster_hash": catalog.roster_hash(REVISION),
        "built_target": 1_000,
        "effective_target": 600,
        "formats": [
            {"id": item.id, "candidates": counts.get(item.id, 0), "exhausted": True}
            for item in catalog.formats
        ],
    }


def publish(tmp_path: Path, rows=ROWS, tweak=None):
    repo = tmp_path / "repo"
    (repo / "data").mkdir(parents=True, exist_ok=True)
    halves = [rows[: len(rows) // 2], rows[len(rows) // 2 :]]
    for index, half in enumerate(halves):
        name = f"train-{index:05d}-of-{len(halves):05d}.jsonl.gz"
        with gzip.open(repo / "data" / name, "wt", encoding="utf-8") as handle:
            for item in half:
                handle.write(json.dumps(item) + "\n")
    meta = sidecar_for(rows, catalog_for(rows))
    if tweak is not None:
        tweak(meta)
    (repo / "manifest.json").write_text(json.dumps(meta))
    return repo


def test_import_reproduces_the_manifest(tmp_path: Path):
    repo = publish(tmp_path)
    destination = tmp_path / ".manifest.sqlite3"

    assets.import_dataset(repo, destination)

    catalog = catalog_for(ROWS)
    manifest = open_manifest(destination, catalog.roster_hash(REVISION))
    batch = manifest.read("core-programming/Python", cursor=0, limit=10)
    assert [item.blob_id for item in batch.items] == ["b0", "b1"]
    assert [item.weight for item in batch.items] == [4, 1]
    assert manifest.capacity("core-programming/Python") == 600
    assert manifest.exhausted("docs-prose-markup/Markdown") is True
    assert manifest.effective_target == 600


def test_import_keeps_row_order_across_shards(tmp_path: Path):
    ordered = [dict(item, blob_id=f"b{index}") for index, item in enumerate(ROWS * 4)]
    repo = publish(tmp_path, rows=ordered)

    assets.import_dataset(repo, tmp_path / ".manifest.sqlite3")

    catalog = catalog_for(ordered)
    manifest = open_manifest(
        tmp_path / ".manifest.sqlite3", catalog.roster_hash(REVISION)
    )
    batch = manifest.read("core-programming/Python", cursor=0, limit=20)
    expected = [
        item["blob_id"] for item in ordered if item["language"] == "Python"
    ]
    assert [item.blob_id for item in batch.items] == expected


def test_import_rejects_a_sidecar_count_mismatch(tmp_path: Path):
    def inflate(meta):
        meta["formats"][0]["candidates"] += 1

    repo = publish(tmp_path, tweak=inflate)

    with pytest.raises(ConfigurationError, match="sidecar"):
        assets.import_dataset(repo, tmp_path / ".manifest.sqlite3")


def test_import_rejects_a_drifted_roster(tmp_path: Path):
    def drift(meta):
        meta["roster_hash"] = "not-the-roster"

    repo = publish(tmp_path, tweak=drift)

    with pytest.raises(ConfigurationError, match="roster"):
        assets.import_dataset(repo, tmp_path / ".manifest.sqlite3")


def test_fetch_imports_through_a_snapshot(tmp_path: Path, monkeypatch):
    repo = publish(tmp_path)
    monkeypatch.setattr(assets, "_snapshot", lambda _repo, _token: str(repo))

    name = assets.fetch_dataset(tmp_path / ".manifest.sqlite3", token=None)

    assert name == assets.assets_repo()
    assert (tmp_path / ".manifest.sqlite3").exists()


def test_fetch_from_an_empty_repo_fails_with_guidance(tmp_path: Path, monkeypatch):
    empty = tmp_path / "empty"
    empty.mkdir()
    monkeypatch.setattr(assets, "_snapshot", lambda _repo, _token: str(empty))

    with pytest.raises(ConfigurationError, match="sidecar"):
        assets.fetch_dataset(tmp_path / ".manifest.sqlite3", token=None)


def test_assets_repo_honours_the_environment(monkeypatch):
    monkeypatch.setenv("SNGRAM_ASSETS_REPO", "other/repo")
    assert assets.assets_repo() == "other/repo"
    monkeypatch.delenv("SNGRAM_ASSETS_REPO")
    assert assets.assets_repo() == assets.DEFAULT_ASSETS_REPO
