"""Published manifest dataset on the Hugging Face Hub."""

from __future__ import annotations

import json
import os
from collections.abc import Callable
from itertools import groupby
from operator import itemgetter
from pathlib import Path

from .catalog import Catalog, build_catalog
from .config import GROUP_LABELS
from .errors import ConfigurationError
from .manifest import ManifestWriter

DEFAULT_ASSETS_REPO = "eliosai/sngram-train"
_MANIFEST_META = "manifest.json"

Report = Callable[[str], None]

_AREA_BY_LABEL = {label: area for area, label in GROUP_LABELS.items()}


def assets_repo() -> str:
    return os.environ.get("SNGRAM_ASSETS_REPO", DEFAULT_ASSETS_REPO)


def fetch_dataset(manifest_path: Path, token: str | None, report: Report | None = None) -> str:
    """Download the published dataset and import it to a local manifest."""

    repo = assets_repo()
    local = Path(_snapshot(repo, token))
    if not (local / _MANIFEST_META).exists():
        raise ConfigurationError(f"{repo} has no published manifest sidecar")
    import_dataset(local, manifest_path, report)
    return repo


def import_dataset(
    dataset_dir: Path, manifest_path: Path, report: Report | None = None
) -> str:
    """Rebuild a SQLite manifest from downloaded jsonl shards."""

    meta = json.loads((dataset_dir / _MANIFEST_META).read_text())
    catalog = _catalog_from(meta)
    roster = catalog.roster_hash(meta["revision"])
    if roster != meta["roster_hash"]:
        raise ConfigurationError("published roster does not match this corpus contract")
    shards = sorted((dataset_dir / "data").glob("train-*.jsonl.gz"))
    if not shards:
        raise ConfigurationError(f"{dataset_dir} has no jsonl shards")
    with ManifestWriter(manifest_path, meta["revision"], roster) as writer:
        for entry in meta["formats"]:
            writer.register(entry["id"], entry["exhausted"])
        for index, shard in enumerate(shards):
            _add_shard(writer, shard)
            if report is not None:
                report(f"imported shard {index + 1}/{len(shards)}")
        writer.set_targets(meta.get("built_target"), meta.get("effective_target"))
        _check_counts(writer, meta["formats"])
    return roster


def _snapshot(repo: str, token: str | None) -> str:
    from huggingface_hub import snapshot_download
    from huggingface_hub.errors import RepositoryNotFoundError

    try:
        return snapshot_download(
            repo,
            repo_type="dataset",
            token=token,
            allow_patterns=["data/*.jsonl.gz", _MANIFEST_META],
        )
    except RepositoryNotFoundError as error:
        raise ConfigurationError(f"{repo} does not exist or this token cannot read it") from error


def _add_shard(writer: ManifestWriter, path: Path) -> None:
    for batch in _read_batches(path):
        _add_batch(writer, batch)


def _read_batches(path: Path):
    import pyarrow as pa
    from pyarrow import json as pajson

    options = pajson.ParseOptions(explicit_schema=_schema())
    with pa.OSFile(str(path), "rb") as file:
        with pa.CompressedInputStream(file, "gzip") as stream:
            table = pajson.read_json(stream, parse_options=options)
    return table.to_batches()


def _schema():
    import pyarrow as pa

    return pa.schema(
        [
            ("group", pa.string()),
            ("language", pa.string()),
            ("extension", pa.string()),
            ("license", pa.string()),
            ("blob_id", pa.string()),
            ("encoding", pa.string()),
            ("length", pa.int64()),
            ("weight", pa.int64()),
        ]
    )


def _add_batch(writer: ManifestWriter, batch) -> None:
    ids = [
        f"{_AREA_BY_LABEL[label]}/{language}"
        for label, language in zip(
            batch.column("group").to_pylist(), batch.column("language").to_pylist()
        )
    ]
    rows = zip(
        batch.column("blob_id").to_pylist(),
        batch.column("encoding").to_pylist(),
        batch.column("length").to_pylist(),
        batch.column("weight").to_pylist(),
        batch.column("extension").to_pylist(),
        batch.column("license").to_pylist(),
    )
    for format_id, group_rows in groupby(zip(ids, rows), key=itemgetter(0)):
        writer.add_rows(format_id, (row for _id, row in group_rows))


def _check_counts(writer: ManifestWriter, formats: list[dict]) -> None:
    for entry in formats:
        if writer.candidates(entry["id"]) != entry["candidates"]:
            raise ConfigurationError(
                f"imported rows for {entry['id']} do not match the published sidecar"
            )


def _catalog_from(meta: dict) -> Catalog:
    configs = sorted({entry["id"].split("/", 1)[1] for entry in meta["formats"]})
    return build_catalog(configs)
