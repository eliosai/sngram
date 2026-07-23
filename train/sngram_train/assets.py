"""Published manifest datasets on the Hugging Face Hub."""

from __future__ import annotations

import os
import tempfile
from pathlib import Path

from . import dataset
from .errors import ConfigurationError

DEFAULT_ASSETS_REPO = "eliosai/sngram-train"

_CARD = """---
license: other
configs:
- config_name: default
  data_files:
  - split: train
    path: {data}/train-*.parquet
---

# sngram-train corpus manifest

Sampled Stack v2 object manifest for sngram weight-table training.
Corpus revision `{revision}`.

Each row is one sampled object to fetch and count. Columns:
`format_id`, `blob_id`, `encoding`, `length`, `weight`.
"""


def assets_repo() -> str:
    return os.environ.get("SNGRAM_ASSETS_REPO", DEFAULT_ASSETS_REPO)


def fetch_dataset(manifest_path: Path, token: str | None) -> str:
    """Download the published dataset and import it to a local manifest."""

    repo = assets_repo()
    local = Path(_snapshot(repo, token))
    if not (local / dataset.MANIFEST_META).exists():
        raise ConfigurationError(
            f"{repo} has no published manifest; "
            "run `sngram manifest build --publish` once"
        )
    dataset.import_dataset(local, manifest_path)
    return repo


def publish_dataset(manifest_path: Path, token: str) -> str:
    """Export the local manifest to Parquet and upload it as a dataset."""

    repo = assets_repo()
    with tempfile.TemporaryDirectory() as tmp:
        out = Path(tmp)
        sidecar = dataset.export_dataset(manifest_path, out)
        _write_card(out, sidecar)
        _upload_folder(repo, out, token)
    return repo


def _write_card(out_dir: Path, sidecar: dict) -> None:
    card = _CARD.format(data=dataset.DATA_DIR, revision=sidecar.get("revision"))
    (out_dir / "README.md").write_text(card)


def _snapshot(repo: str, token: str | None) -> str:
    from huggingface_hub import snapshot_download
    from huggingface_hub.errors import RepositoryNotFoundError

    try:
        return snapshot_download(
            repo,
            repo_type="dataset",
            token=token,
            allow_patterns=[f"{dataset.DATA_DIR}/*", dataset.MANIFEST_META],
        )
    except RepositoryNotFoundError as error:
        raise ConfigurationError(
            f"{repo} does not exist; run `sngram manifest build --publish` once"
        ) from error


def _upload_folder(repo: str, folder: Path, token: str) -> None:
    from huggingface_hub import HfApi

    api = HfApi(token=token)
    api.create_repo(repo, repo_type="dataset", private=True, exist_ok=True)
    api.upload_folder(folder_path=str(folder), repo_id=repo, repo_type="dataset")
