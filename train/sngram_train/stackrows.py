"""Pinned, row-batched Stack v2 metadata source."""

from __future__ import annotations

import hashlib
from collections.abc import Iterator

from .config import (
    STACK_V2_DOC_MAX_BYTES,
    STACK_V2_METADATA_REPO,
    STACK_V2_REQUIRED_COLUMNS,
)
from .errors import ConfigurationError
from .sampling import SAMPLE_FLOOR

METADATA_BATCH_ROWS = 16_384
METADATA_BLOCK_BYTES = 64 * 1024 * 1024


class HuggingFaceRows:
    """Stack v2 metadata reader pinned to one dataset revision."""

    def __init__(self, token: str, revision: str | None = None) -> None:
        from huggingface_hub import HfApi, HfFileSystem

        self.token = token
        self.revision = (
            revision or HfApi(token=token).dataset_info(STACK_V2_METADATA_REPO).sha
        )
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
        with self._fs.open(
            url,
            "rb",
            cache_type="readahead",
            block_size=METADATA_BLOCK_BYTES,
        ) as handle:
            parquet = pq.ParquetFile(handle, pre_buffer=False)
            _validate_columns(parquet.schema_arrow.names)
            seed = int.from_bytes(_digest(f"{self.revision}:{path}"), "little")
            yield from _shard_groups(parquet, seed, start_row, shard_index)


def _shard_groups(parquet, seed: int, start_row: int, shard_index: int):
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
        yield from _resume_batches(batches, seed, row_offset, start_row, shard_index)
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
