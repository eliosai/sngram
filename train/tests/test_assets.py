import shutil
from pathlib import Path

import pytest

from sngram_train import assets, dataset
from sngram_train.errors import ConfigurationError
from tests.test_dataset import build_small


def route_through_local_repo(monkeypatch, repo_dir: Path):
    def fake_upload(repo, folder, token):
        shutil.copytree(folder, repo_dir, dirs_exist_ok=True)

    def fake_snapshot(repo, token):
        return str(repo_dir)

    monkeypatch.setattr(assets, "_upload_folder", fake_upload)
    monkeypatch.setattr(assets, "_snapshot", fake_snapshot)


def test_publish_then_fetch_reproduces_the_manifest(tmp_path: Path, monkeypatch):
    build_small(tmp_path / "publisher.sqlite3")
    repo_dir = tmp_path / "repo"
    route_through_local_repo(monkeypatch, repo_dir)

    assets.publish_dataset(tmp_path / "publisher.sqlite3", token="write-token")
    assert (repo_dir / "README.md").exists()
    assert (repo_dir / dataset.MANIFEST_META).exists()

    dest = tmp_path / "reader" / ".manifest.sqlite3"
    repo = assets.fetch_dataset(dest, token="read-token")

    assert repo == assets.assets_repo()
    assert dest.exists()
    from sngram_train.manifest import stored_format_ids

    assert "core-programming/Python" in stored_format_ids(dest)


def test_fetch_from_an_empty_repo_guides_to_build(tmp_path: Path, monkeypatch):
    empty = tmp_path / "empty"
    empty.mkdir()
    monkeypatch.setattr(assets, "_snapshot", lambda repo, token: str(empty))

    with pytest.raises(ConfigurationError, match="sngram manifest build --publish"):
        assets.fetch_dataset(tmp_path / ".manifest.sqlite3", token=None)


def test_assets_repo_honours_the_environment(monkeypatch):
    monkeypatch.setenv("SNGRAM_ASSETS_REPO", "other/repo")
    assert assets.assets_repo() == "other/repo"
    monkeypatch.delenv("SNGRAM_ASSETS_REPO")
    assert assets.assets_repo() == assets.DEFAULT_ASSETS_REPO


def test_card_declares_the_default_parquet_config(tmp_path: Path):
    assets._write_card(tmp_path, {"revision": "abc123"})

    card = (tmp_path / "README.md").read_text()
    assert "data_files:" in card
    assert f"{dataset.DATA_DIR}/train-*.parquet" in card
    assert "abc123" in card
