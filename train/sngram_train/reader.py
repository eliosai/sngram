"""Pruned batch reads over one stack-v3 parquet shard."""

from __future__ import annotations

from collections.abc import Iterator
from dataclasses import dataclass

import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

from .errors import ConfigurationError

BATCH_ROWS = 64

_FIELDS = ("content", "language", "is_vendor")


@dataclass(frozen=True)
class Sifted:
    """One decoded repository batch reduced to countable content"""

    content: pa.RecordBatch
    repos: int
    files: int
    vendor_files: int
    lang_bytes: dict[str, int]


class ShardReader:
    """Row group batches of one shard, pruned and vendor filtered."""

    def __init__(self, handle) -> None:
        self._file = pq.ParquetFile(handle)
        self._columns = _pruned_columns(self._file.schema)

    @property
    def row_groups(self) -> int:
        return self._file.metadata.num_row_groups

    def batches(self, row_group: int, skip_rows: int) -> Iterator[Sifted]:
        pending = skip_rows
        raw = self._file.iter_batches(
            BATCH_ROWS, row_groups=[row_group], columns=self._columns, use_threads=False
        )
        for batch in raw:
            if pending:
                pending = _drop(pending, batch.num_rows)
                continue
            yield _sift(batch)


def _pruned_columns(schema) -> list[str]:
    paths = [schema.column(index).path for index in range(len(schema.names))]
    wanted = [
        path
        for path in paths
        if path.startswith("files.") and path.rsplit(".", 1)[-1] in _FIELDS
    ]
    if len(wanted) != len(_FIELDS):
        raise ConfigurationError("shard schema is missing the files content fields")
    return wanted


def _drop(pending: int, rows: int) -> int:
    if rows > pending:
        raise ConfigurationError(
            "checkpoint rows do not align with shard batches; "
            "pass --no-resume or a fresh --mint-dir to restart"
        )
    return pending - rows


def _sift(batch: pa.RecordBatch) -> Sifted:
    files = pc.list_flatten(batch.column(0))
    vendor = pc.fill_null(files.field("is_vendor"), False)
    keep = pc.and_(pc.invert(vendor), pc.is_valid(files.field("content")))
    content = pc.filter(files.field("content"), keep)
    languages = pc.filter(files.field("language"), keep)
    return Sifted(
        pa.record_batch([content], names=["content"]),
        batch.num_rows,
        len(content),
        pc.sum(vendor).as_py() or 0,
        _language_bytes(languages, content),
    )


def _language_bytes(languages: pa.Array, content: pa.Array) -> dict[str, int]:
    pairs = pa.table({"language": languages, "nbytes": pc.binary_length(content)})
    grouped = pairs.group_by("language").aggregate([("nbytes", "sum")])
    named = zip(
        grouped.column("language").to_pylist(), grouped.column("nbytes_sum").to_pylist()
    )
    return {language or "unknown": total for language, total in named}
