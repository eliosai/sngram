"""Countable batches from one spooled shard, by layout."""

from __future__ import annotations

import gzip
import json
from collections.abc import Iterator

import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

from .corpus import Shard, Sifted
from .errors import ConfigurationError

BATCH_ROWS = 64

_READ_BUFFER = 4 * 2**20

_STACK_FIELDS = ("content", "language", "is_vendor", "size_bytes")

_LINE_BATCH_ROWS = 512

_LINE_BATCH_BYTES = 4 * 2**20


def open_reader(handle, shard: Shard):
    """The reader for one shard's layout."""

    layout = shard.reading.layout
    if layout == "stack-files":
        return StackReader(handle)
    if layout == "parquet-column":
        return ColumnReader(handle, shard.reading.field)
    return LineReader(handle, shard)


class StackReader:
    """Row group batches of one nested stack shard, pruned to countable columns."""

    def __init__(self, handle) -> None:
        self._file = pq.ParquetFile(
            handle,
            buffer_size=_READ_BUFFER,
            binary_type=pa.binary_view(),
            pre_buffer=False,
        )
        self._columns = _nested_columns(self._file.schema)

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
            sifted = _sift(batch)
            # free each batch before the next decode
            del batch
            yield sifted
            del sifted
        pa.default_memory_pool().release_unused()


class ColumnReader:
    """Row group batches of one flat shard, pruned to its text column."""

    def __init__(self, handle, field: str) -> None:
        self._file = pq.ParquetFile(handle, buffer_size=_READ_BUFFER, pre_buffer=False)
        names = self._file.schema_arrow.names
        if field not in names:
            raise ConfigurationError(f"shard has no {field!r} column; columns are {names}")
        self._field = field

    @property
    def row_groups(self) -> int:
        return self._file.metadata.num_row_groups

    def batches(self, row_group: int, skip_rows: int) -> Iterator[Sifted]:
        pending = skip_rows
        raw = self._file.iter_batches(
            BATCH_ROWS, row_groups=[row_group], columns=[self._field], use_threads=False
        )
        for batch in raw:
            if pending:
                pending = _drop(pending, batch.num_rows)
                continue
            sifted = _column(batch)
            del batch
            yield sifted
            del sifted
        pa.default_memory_pool().release_unused()


class LineReader:
    """Batches of one JSON-lines shard, read as a single row group."""

    def __init__(self, handle, shard: Shard) -> None:
        self._lines = gzip.open(handle, "rb") if shard.path.endswith(".gz") else handle
        self._field = shard.reading.field

    @property
    def row_groups(self) -> int:
        return 1

    def batches(self, row_group: int, skip_rows: int) -> Iterator[Sifted]:
        pending = skip_rows
        batch = _LineBatch(self._field)
        for line in self._lines:
            if pending:
                pending -= 1
                continue
            batch.add(line)
            if batch.full():
                yield batch.take()
        if batch.rows:
            yield batch.take()


class _LineBatch:
    """Text values accumulated from JSON lines until a batch is worth counting"""

    def __init__(self, field: str) -> None:
        self._field = field
        self._values: list[str] = []
        self._size = 0
        self.rows = 0

    def add(self, line: bytes) -> None:
        self.rows += 1
        value = json.loads(line).get(self._field)
        if isinstance(value, str):
            self._values.append(value)
            self._size += len(value)

    def full(self) -> bool:
        return len(self._values) >= _LINE_BATCH_ROWS or self._size >= _LINE_BATCH_BYTES

    def take(self) -> Sifted:
        column = pa.array(self._values, type=pa.large_string())
        sifted = Sifted(
            pa.record_batch([column], names=["content"]), self.rows, len(self._values)
        )
        self._values, self._size, self.rows = [], 0, 0
        return sifted


def _nested_columns(schema) -> list[str]:
    paths = [schema.column(index).path for index in range(len(schema.names))]
    wanted = [
        path
        for path in paths
        if path.startswith("files.") and path.rsplit(".", 1)[-1] in _STACK_FIELDS
    ]
    if len(wanted) != len(_STACK_FIELDS):
        raise ConfigurationError("shard schema is missing the files content fields")
    return wanted


def _drop(pending: int, rows: int) -> int:
    if rows > pending:
        raise ConfigurationError(
            "checkpoint rows do not align with shard batches; "
            "pass --no-resume or a fresh --mint-dir to restart"
        )
    return pending - rows


def _column(batch: pa.RecordBatch) -> Sifted:
    content = batch.column(0)
    if content.null_count:
        content = pc.drop_null(content)
    return Sifted(
        pa.record_batch([content], names=["content"]), batch.num_rows, len(content)
    )


def _file_window(column: pa.Array) -> pa.Array:
    """Zero-copy view of the file structs behind one batch"""

    offsets = column.offsets
    start = offsets[0].as_py()
    return column.values.slice(start, offsets[-1].as_py() - start)


def _sift(batch: pa.RecordBatch) -> Sifted:
    files = _file_window(batch.column(0))
    vendor = pc.fill_null(files.field("is_vendor"), False)
    content = files.field("content")
    languages = files.field("language")
    sizes = files.field("size_bytes")
    if content.null_count:
        keep = pc.is_valid(content)
        content = pc.filter(content, keep)
        languages = pc.filter(languages, keep)
        sizes = pc.filter(sizes, keep)
    return Sifted(
        pa.record_batch([content], names=["content"]),
        batch.num_rows,
        len(content),
        pc.sum(vendor).as_py() or 0,
        _language_bytes(languages, sizes),
    )


def _language_bytes(languages: pa.Array, sizes: pa.Array) -> dict[str, int]:
    pairs = pa.table({"language": pc.cast(languages, pa.string()), "nbytes": sizes})
    grouped = pairs.group_by("language").aggregate([("nbytes", "sum")])
    named = zip(
        grouped.column("language").to_pylist(), grouped.column("nbytes_sum").to_pylist()
    )
    return {language or "unknown": total or 0 for language, total in named}
