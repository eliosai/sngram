"""Stack v2 metadata inventory for the durable training manifest."""

from __future__ import annotations

import hashlib
import time
from collections.abc import Iterator, Mapping
from pathlib import Path
from typing import Callable, Protocol

from .catalog import Catalog
from .config import (
    STACK_V2_DOC_MAX_BYTES,
    STACK_V2_MAX_BYTES,
    STACK_V2_METADATA_REPO,
    STACK_V2_REQUIRED_COLUMNS,
)
from .distribution import apportion, waterfill
from .errors import ConfigurationError, CorpusExhausted
from .manifest import Candidate, ManifestBuilder
from .sampling import SAMPLE_FLOOR, sample_weight

METADATA_BATCH_ROWS = 16_384


class StackRows(Protocol):
    revision: str

    def iter_rows(
        self, config: str, cursor: tuple[int, int] = (0, 0)
    ) -> Iterator[dict[str, object]]: ...


def build_stack_manifest(
    path: Path,
    catalog: Catalog,
    rows: StackRows,
    on_config: Callable[[tuple[str, int, int, float]], None] | None = None,
    *,
    target: int | None = None,
    area_weights: Mapping[str, int] | None = None,
) -> str:
    """Build a sampled manifest up to exact adaptive format goals."""

    roster_hash = catalog.roster_hash(rows.revision, target)
    with ManifestBuilder(path, rows.revision, roster_hash) as builder:
        for item in catalog.formats:
            builder.register(item.id)
        if target is None:
            _static_inventory(builder, catalog, rows, on_config)
        else:
            if area_weights is None:
                raise ValueError("area weights are required for target-bounded inventory")
            _adaptive_inventory(
                builder, catalog, rows, target, area_weights, on_config
            )
    return roster_hash


def _static_inventory(
    builder: ManifestBuilder,
    catalog: Catalog,
    rows: StackRows,
    on_config: Callable[[tuple[str, int, int, float]], None] | None,
) -> None:
    limits = {item.id: item.cap_bytes for item in catalog.formats}
    for config in catalog.configs:
        if builder.is_complete(config):
            continue
        _scan_and_commit(builder, catalog, rows, config, limits, on_config)


def _adaptive_inventory(
    builder: ManifestBuilder,
    catalog: Catalog,
    rows: StackRows,
    target: int,
    area_weights: Mapping[str, int],
    on_config: Callable[[tuple[str, int, int, float]], None] | None,
) -> None:
    while True:
        goals = _inventory_goals(builder, catalog, target, area_weights)
        configs = _pending_configs(builder, catalog, goals)
        if not configs:
            return
        for config in configs:
            _scan_and_commit(builder, catalog, rows, config, goals, on_config)


def _scan_and_commit(
    builder: ManifestBuilder,
    catalog: Catalog,
    rows: StackRows,
    config: str,
    limits: Mapping[str, int],
    on_config: Callable[[tuple[str, int, int, float]], None] | None,
) -> None:
    started = time.monotonic()
    accepted, effective, exhausted, cursor = _scan_config(
        builder, catalog, rows, config, limits
    )
    for format_id in exhausted:
        builder.set_exhausted(format_id)
    builder.finish_config(config, cursor)
    if on_config is not None:
        on_config((config, accepted, effective, time.monotonic() - started))


def _scan_config(
    builder: ManifestBuilder,
    catalog: Catalog,
    rows: StackRows,
    config: str,
    limits: Mapping[str, int],
) -> tuple[int, int, set[str], tuple[int, int]]:
    formats = [item for item in catalog.formats if item.config == config]
    add_limits = {
        item.id: item.cap_bytes if len(formats) > 1 else limits[item.id]
        for item in formats
    }
    cursor = builder.cursor(config)
    if all(_reached(builder, item.id, limits[item.id]) for item in formats):
        return 0, 0, set(), cursor
    accepted = 0
    effective = 0
    for row in rows.iter_rows(config, cursor):
        cursor = tuple(row.get("_source_cursor", cursor))
        format_id = catalog.route(config, row)
        spec = catalog.format(format_id)
        if skip_reason(row, spec.area) is not None:
            continue
        candidate = _candidate(format_id, row)
        if candidate is None:
            continue
        if _reached(builder, format_id, add_limits[format_id]):
            continue
        builder.add(candidate)
        amount = candidate.length * candidate.weight
        accepted += 1
        effective += amount
        if all(_reached(builder, item.id, limits[item.id]) for item in formats):
            return accepted, effective, set(), cursor
    exhausted = {
        item.id
        for item in formats
        if not _reached(builder, item.id, limits[item.id])
    }
    return accepted, effective, exhausted, cursor


def _reached(builder: ManifestBuilder, format_id: str, limit: int) -> bool:
    return builder.is_exhausted(format_id) or builder.capacity(format_id) >= limit


def _pending_configs(
    builder: ManifestBuilder, catalog: Catalog, goals: Mapping[str, int]
) -> list[str]:
    return [
        config
        for config in catalog.configs
        if any(
            not _reached(builder, item.id, goals[item.id])
            for item in catalog.formats
            if item.config == config
        )
    ]


def _inventory_goals(
    builder: ManifestBuilder,
    catalog: Catalog,
    target: int,
    area_weights: Mapping[str, int],
) -> dict[str, int]:
    targets = apportion(target, area_weights)
    goals: dict[str, int] = {}
    for area, amount in targets.items():
        formats = [item for item in catalog.formats if item.area == area]
        capacities = {
            item.id: builder.capacity(item.id) if builder.is_exhausted(item.id)
            else item.cap_bytes
            for item in formats
        }
        try:
            goals.update(waterfill(amount, capacities))
        except ValueError as error:
            raise CorpusExhausted(f"area {area} has less than {amount} bytes") from error
    return goals


def _candidate(format_id: str, row: dict[str, object]) -> Candidate | None:
    length = int(row["length_bytes"])
    content_id = str(row["content_id"])
    weight = row.get("_sample_weight")
    weight = int(weight) if weight is not None else sample_weight(content_id, length)
    if weight is None:
        return None
    return Candidate(
        format_id,
        str(row["blob_id"]),
        str(row["src_encoding"]),
        length,
        weight,
    )


def skip_reason(row: dict[str, object], area: str) -> str | None:
    """Validate one metadata row without imposing a minimum file size."""

    if row.get("is_vendor") is True:
        return "vendor"
    if row.get("is_generated") is True:
        return "generated"
    for field in ("blob_id", "content_id", "src_encoding", "language"):
        if not row.get(field):
            return f"missing_{field}"
    try:
        length = int(row.get("length_bytes") or 0)
    except (TypeError, ValueError):
        return "bad_length"
    if length <= 0:
        return "empty"
    limit = STACK_V2_DOC_MAX_BYTES if area == "docs-prose-markup" else STACK_V2_MAX_BYTES
    return "oversize" if length > limit else None


class HuggingFaceRows:
    """Pinned, row-batched Stack v2 metadata source."""

    def __init__(self, token: str) -> None:
        from huggingface_hub import HfApi, HfFileSystem

        self.token = token
        self.revision = HfApi(token=token).dataset_info(STACK_V2_METADATA_REPO).sha
        self._fs = HfFileSystem(token=token)
        self._files: dict[str, list[str]] | None = None

    def configs(self) -> list[str]:
        return sorted(self._metadata_files())

    def iter_rows(
        self, config: str, cursor: tuple[int, int] = (0, 0)
    ) -> Iterator[dict[str, object]]:
        for shard_index, shard in enumerate(self._shards(config)[cursor[0] :], cursor[0]):
            start_row = cursor[1] if shard_index == cursor[0] else 0
            yield from self._read_shard(shard, shard_index, start_row)

    def _shards(self, config: str) -> list[str]:
        paths = self._metadata_files().get(config, [])
        return sorted(paths, key=lambda path: _digest(f"{self.revision}:{path}"))

    def _metadata_files(self) -> dict[str, list[str]]:
        if self._files is not None:
            return self._files
        prefix = f"datasets/{STACK_V2_METADATA_REPO}@{self.revision}/data/"
        paths = self._fs.glob(f"{prefix}*/train-*.parquet")
        files: dict[str, list[str]] = {}
        for path in paths:
            marker = f"@{self.revision}/data/"
            if marker not in path:
                continue
            config = path.split(marker, 1)[1].split("/", 1)[0]
            files.setdefault(config, []).append(path)
        if not files:
            raise RuntimeError("no pinned Stack metadata shards were found")
        self._files = files
        return files

    def _read_shard(
        self, path: str, shard_index: int, start_row: int
    ) -> Iterator[dict[str, object]]:
        import pyarrow.parquet as pq

        url = path if path.startswith("hf://") else f"hf://{path}"
        with self._fs.open(url, "rb", cache_type="none") as handle:
            parquet = pq.ParquetFile(handle, pre_buffer=False)
            _validate_columns(parquet.schema_arrow.names)
            seed = int.from_bytes(_digest(f"{self.revision}:{path}"), "little")
            row_offset = 0
            for row_group in range(parquet.num_row_groups):
                rows = parquet.metadata.row_group(row_group).num_rows
                if row_offset + rows <= start_row:
                    row_offset += rows
                    continue
                batches = parquet.iter_batches(
                    batch_size=METADATA_BATCH_ROWS,
                    columns=list(STACK_V2_REQUIRED_COLUMNS),
                    row_groups=[row_group],
                )
                yield from _resume_batches(
                    batches, seed, row_offset, start_row, shard_index
                )
                row_offset += rows


def _sampled_rows(batch, seed: int, offset: int) -> Iterator[dict[str, object]]:
    import numpy as np
    import pyarrow as pa

    valid = _valid_rows(batch)
    if not valid.any():
        return
    lengths = batch.column("length_bytes").to_numpy(zero_copy_only=False)[valid]
    weights = _weights(lengths)
    ordinals = np.arange(offset, offset + batch.num_rows, dtype=np.uint64)[valid]
    selected = (_mix(ordinals ^ np.uint64(seed)) & (weights - 1)) == 0
    indices = np.flatnonzero(valid)[selected]
    if not len(indices):
        return
    sampled = batch.take(pa.array(indices))
    columns = sampled.to_pydict()
    for index, weight in enumerate(weights[selected]):
        row = {name: values[index] for name, values in columns.items()}
        row["_sample_weight"] = int(weight)
        row["_source_row"] = offset + int(indices[index]) + 1
        yield row


def _resume_batches(batches, seed: int, offset: int, start: int, shard: int):
    for batch in batches:
        end = offset + batch.num_rows
        if end <= start:
            offset = end
            continue
        skip = max(start - offset, 0)
        for row in _sampled_rows(batch.slice(skip), seed, offset + skip):
            row["_source_cursor"] = (shard, row.pop("_source_row"))
            yield row
        offset = end


def _valid_rows(batch):
    import numpy as np

    lengths = batch.column("length_bytes").to_numpy(zero_copy_only=False)
    vendor = batch.column("is_vendor").to_numpy(zero_copy_only=False)
    generated = batch.column("is_generated").to_numpy(zero_copy_only=False)
    valid = (lengths > 0) & (lengths <= STACK_V2_DOC_MAX_BYTES)
    valid &= ~np.asarray(vendor, dtype=bool) & ~np.asarray(generated, dtype=bool)
    for name in ("blob_id", "content_id", "src_encoding", "language"):
        valid &= np.asarray(batch.column(name).is_valid())
    return valid


def _weights(lengths):
    import numpy as np

    ratio = (SAMPLE_FLOOR + lengths - 1) // lengths
    powers = np.ceil(np.log2(ratio)).astype(np.uint64)
    return np.left_shift(np.uint64(1), powers)


def _mix(values):
    import numpy as np

    values = values + np.uint64(0x9E3779B97F4A7C15)
    values = (values ^ (values >> np.uint64(30))) * np.uint64(0xBF58476D1CE4E5B9)
    values = (values ^ (values >> np.uint64(27))) * np.uint64(0x94D049BB133111EB)
    return values ^ (values >> np.uint64(31))


def _digest(value: str) -> bytes:
    return hashlib.blake2b(value.encode(), digest_size=8, person=b"sngram-v3").digest()


def _validate_columns(columns: list[str]) -> None:
    missing = sorted(set(STACK_V2_REQUIRED_COLUMNS) - set(columns))
    if missing:
        raise ConfigurationError(f"Stack metadata is missing columns: {missing}")
