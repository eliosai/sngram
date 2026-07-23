"""Convert the SQLite manifest to and from a Parquet dataset."""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path

CANDIDATE_COLUMNS = ("format_id", "blob_id", "encoding", "length", "weight")
DATA_DIR = "data"
MANIFEST_META = "manifest.json"
SHARD_ROWS = 4_000_000
READ_BATCH = 262_144


def export_dataset(manifest_path: Path, out_dir: Path) -> dict:
    """Write a manifest's candidates and metadata as a Parquet dataset."""

    data_dir = out_dir / DATA_DIR
    data_dir.mkdir(parents=True, exist_ok=True)
    connection = sqlite3.connect(f"file:{manifest_path}?mode=ro", uri=True)
    try:
        formats = _read_formats(connection)
        meta = dict(connection.execute("SELECT key, value FROM metadata"))
        encodings = dict(connection.execute("SELECT key, name FROM encodings"))
        keys = {row["format_key"]: row["id"] for row in formats}
        shards = max(-(-sum(row["candidates"] for row in formats) // SHARD_ROWS), 1)
        rows = connection.execute(
            "SELECT format_key, blob_id, encoding_key, length, weight "
            "FROM candidates ORDER BY format_key, sequence"
        )
        _write_shards(rows, data_dir, shards, keys, encodings)
    finally:
        connection.close()
    sidecar = _sidecar(meta, formats)
    (out_dir / MANIFEST_META).write_text(json.dumps(sidecar, indent=2))
    return sidecar


def import_dataset(dataset_dir: Path, manifest_path: Path) -> str:
    """Rebuild a SQLite manifest from a downloaded Parquet dataset."""

    from .manifest import ManifestBuilder

    meta = json.loads((dataset_dir / MANIFEST_META).read_text())
    catalog = _catalog_from(meta)
    roster = catalog.roster_hash(meta["revision"])
    manifest_path.parent.mkdir(parents=True, exist_ok=True)
    manifest_path.with_suffix(manifest_path.suffix + ".tmp").unlink(missing_ok=True)
    files = sorted((dataset_dir / DATA_DIR).glob("*.parquet"))
    with ManifestBuilder(manifest_path, meta["revision"], roster) as builder:
        for item in catalog.formats:
            builder.register(item.id)
        for file in files:
            _add_shard(builder, file)
        _apply_meta(builder, meta)
    return roster


def _read_formats(connection: sqlite3.Connection) -> list[dict]:
    cursor = connection.execute(
        "SELECT format_key, id, candidates, exhausted FROM formats ORDER BY id"
    )
    return [
        {"format_key": key, "id": fid, "candidates": count, "exhausted": bool(done)}
        for key, fid, count, done in cursor
    ]


def _sidecar(meta: dict, formats: list[dict]) -> dict:
    return {
        "revision": meta.get("revision"),
        "roster_hash": meta.get("roster_hash"),
        "built_target": _int_or_none(meta.get("built_target")),
        "effective_target": _int_or_none(meta.get("effective_target")),
        "formats": [{"id": row["id"], "exhausted": row["exhausted"]} for row in formats],
    }


def _write_shards(rows, data_dir: Path, shards: int, keys: dict, encodings: dict) -> None:
    import pyarrow.parquet as pq

    writer = None
    shard = in_shard = 0
    for batch in _fetch_batches(rows, READ_BATCH):
        table = _to_table(batch, keys, encodings)
        offset = 0
        while offset < table.num_rows:
            if writer is None:
                path = data_dir / _shard_name(shard, shards)
                writer = pq.ParquetWriter(path, table.schema, compression="zstd")
                in_shard = 0
            chunk = table.slice(offset, SHARD_ROWS - in_shard)
            writer.write_table(chunk)
            in_shard += chunk.num_rows
            offset += chunk.num_rows
            if in_shard >= SHARD_ROWS:
                writer.close()
                writer, shard = None, shard + 1
    if writer is not None:
        writer.close()


def _fetch_batches(rows, size: int):
    while True:
        batch = rows.fetchmany(size)
        if not batch:
            return
        yield batch


def _to_table(batch, keys: dict, encodings: dict):
    import pyarrow as pa

    from .manifest import _unpack_blob_id

    columns: dict[str, list] = {name: [] for name in CANDIDATE_COLUMNS}
    for format_key, blob, encoding_key, length, weight in batch:
        columns["format_id"].append(keys[format_key])
        columns["blob_id"].append(_unpack_blob_id(blob))
        columns["encoding"].append(encodings[encoding_key])
        columns["length"].append(length)
        columns["weight"].append(weight)
    return pa.table(
        {
            "format_id": pa.array(columns["format_id"], pa.string()),
            "blob_id": pa.array(columns["blob_id"], pa.string()),
            "encoding": pa.array(columns["encoding"], pa.string()),
            "length": pa.array(columns["length"], pa.int64()),
            "weight": pa.array(columns["weight"], pa.int64()),
        }
    )


def _add_shard(builder, file: Path) -> None:
    import pyarrow.parquet as pq

    from .manifest import Candidate

    columns = pq.read_table(file, columns=list(CANDIDATE_COLUMNS)).to_pydict()
    for values in zip(*(columns[name] for name in CANDIDATE_COLUMNS)):
        builder.add(Candidate(*values))


def _apply_meta(builder, meta: dict) -> None:
    for entry in meta["formats"]:
        if entry["exhausted"]:
            builder.set_exhausted(entry["id"])
    if meta.get("built_target") is not None:
        builder.set_built_target(meta["built_target"])
    if meta.get("effective_target") is not None:
        builder.set_effective_target(meta["effective_target"])


def _catalog_from(meta: dict):
    from .catalog import build_catalog

    configs = sorted({entry["id"].split("/", 1)[1] for entry in meta["formats"]})
    return build_catalog(configs)


def _shard_name(shard: int, shards: int) -> str:
    return f"train-{shard:05d}-of-{shards:05d}.parquet"


def _int_or_none(value: object) -> int | None:
    return int(value) if value is not None else None
