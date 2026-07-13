"""Config scanning that feeds candidates into a manifest inventory."""

from __future__ import annotations

import sqlite3
import tempfile
import time
from collections.abc import Iterator, Mapping
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol

from .catalog import Catalog
from .config import STACK_V2_DOC_MAX_BYTES, STACK_V2_MAX_BYTES
from .manifest import Candidate, ManifestBuilder
from .sampling import sample_weight

SCAN_REPORT_ROWS = 65_536


class StackRows(Protocol):
    revision: str

    def iter_rows(
        self, config: str, cursor: tuple[int, int] = (0, 0)
    ) -> Iterator[dict[str, object]]: ...


class Inventory(Protocol):
    def add(self, candidate: Candidate) -> None: ...
    def capacity(self, format_id: str) -> int: ...
    def is_exhausted(self, format_id: str) -> bool: ...
    def cursor(self, config: str) -> tuple[int, int]: ...


class ScanReport(Protocol):
    def started(self, config: str) -> None: ...
    def scanned(self, config: str, rows: int, accepted_bytes: int) -> None: ...
    def finished(
        self, config: str, accepted: int, effective: int, seconds: float
    ) -> None: ...


@dataclass(frozen=True)
class _ScanResult:
    config: str
    path: Path
    accepted: int
    effective: int
    exhausted: set[str]
    cursor: tuple[int, int]
    seconds: float


class _ScanSpool:
    """Bounded-memory candidate spool owned by one metadata worker."""

    def __init__(
        self,
        directory: Path,
        capacities: Mapping[str, int],
        exhausted: Mapping[str, bool],
        cursor: tuple[int, int],
    ) -> None:
        handle = tempfile.NamedTemporaryFile(
            prefix=".manifest-scan-", suffix=".sqlite3", dir=directory, delete=False
        )
        self.path = Path(handle.name)
        handle.close()
        self._connection = sqlite3.connect(self.path)
        self._connection.execute(_SPOOL_SCHEMA)
        self._capacities = dict(capacities)
        self._exhausted = dict(exhausted)
        self._cursor = cursor
        self._sequence = 0
        self._buffer: list[tuple[object, ...]] = []

    def add(self, candidate: Candidate) -> None:
        self._buffer.append(
            (
                self._sequence,
                candidate.format_id,
                candidate.blob_id,
                candidate.encoding,
                candidate.length,
                candidate.weight,
            )
        )
        self._sequence += 1
        self._capacities[candidate.format_id] += candidate.length * candidate.weight
        if len(self._buffer) >= 8192:
            self._flush()

    def capacity(self, format_id: str) -> int:
        return self._capacities.get(format_id, 0)

    def is_exhausted(self, format_id: str) -> bool:
        return self._exhausted.get(format_id, False)

    def cursor(self, _config: str) -> tuple[int, int]:
        return self._cursor

    def close(self) -> None:
        self._flush()
        self._connection.commit()
        self._connection.close()

    def abort(self) -> None:
        self._connection.close()
        self.path.unlink(missing_ok=True)

    def _flush(self) -> None:
        self._connection.executemany(
            "INSERT INTO candidates VALUES (?, ?, ?, ?, ?, ?)", self._buffer
        )
        self._buffer.clear()


def scan_configs(
    builder: ManifestBuilder,
    catalog: Catalog,
    rows: StackRows,
    configs: list[str],
    limits: Mapping[str, int],
    report: ScanReport | None,
    workers: int,
) -> None:
    """Scan configs into the builder, spooling in parallel when allowed."""

    if workers <= 1 or len(configs) == 1:
        for config in configs:
            scan_and_commit(builder, catalog, rows, config, limits, report)
        return
    failure: BaseException | None = None
    with ThreadPoolExecutor(max_workers=workers) as pool:
        futures = _submit_scans(pool, builder, catalog, rows, configs, limits, report)
        for future in as_completed(futures):
            try:
                result = future.result()
                if failure is None:
                    _merge_scan(builder, result, report)
                else:
                    result.path.unlink(missing_ok=True)
            except BaseException as error:
                if failure is None:
                    failure = error
                    for pending in futures:
                        pending.cancel()
    if failure is not None:
        raise failure


def _submit_scans(pool, builder, catalog, rows, configs, limits, report):
    capacities = {item.id: builder.capacity(item.id) for item in catalog.formats}
    exhausted = {item.id: builder.is_exhausted(item.id) for item in catalog.formats}
    return [
        pool.submit(
            _scan_to_spool,
            builder.path.parent,
            catalog,
            rows,
            config,
            limits,
            capacities,
            exhausted,
            builder.cursor(config),
            report,
        )
        for config in configs
    ]


def _scan_to_spool(
    directory: Path,
    catalog: Catalog,
    rows: StackRows,
    config: str,
    limits: Mapping[str, int],
    capacities: Mapping[str, int],
    exhausted: Mapping[str, bool],
    cursor: tuple[int, int],
    report: ScanReport | None,
) -> _ScanResult:
    started = time.monotonic()
    spool = _ScanSpool(directory, capacities, exhausted, cursor)
    try:
        accepted, effective, done, final_cursor = _scan_config(
            spool, catalog, rows, config, limits, report
        )
        spool.close()
    except BaseException:
        spool.abort()
        raise
    return _ScanResult(
        config,
        spool.path,
        accepted,
        effective,
        done,
        final_cursor,
        time.monotonic() - started,
    )


def _merge_scan(builder, result, report) -> None:
    try:
        with sqlite3.connect(result.path) as connection:
            rows = connection.execute(
                "SELECT format_id, blob_id, encoding, length, weight "
                "FROM candidates ORDER BY sequence"
            )
            for row in rows:
                builder.add(Candidate(*row))
        for format_id in result.exhausted:
            builder.set_exhausted(format_id)
        builder.finish_config(result.config, result.cursor)
    finally:
        result.path.unlink(missing_ok=True)
    if report is not None:
        report.finished(result.config, result.accepted, result.effective, result.seconds)


def scan_and_commit(
    builder: ManifestBuilder,
    catalog: Catalog,
    rows: StackRows,
    config: str,
    limits: Mapping[str, int],
    report: ScanReport | None,
) -> None:
    started = time.monotonic()
    accepted, effective, exhausted, cursor = _scan_config(
        builder, catalog, rows, config, limits, report
    )
    for format_id in exhausted:
        builder.set_exhausted(format_id)
    builder.finish_config(config, cursor)
    if report is not None:
        report.finished(config, accepted, effective, time.monotonic() - started)


def _scan_config(
    builder: Inventory,
    catalog: Catalog,
    rows: StackRows,
    config: str,
    limits: Mapping[str, int],
    report: ScanReport | None = None,
) -> tuple[int, int, set[str], tuple[int, int]]:
    formats = [item for item in catalog.formats if item.config == config]
    add_limits = _add_limits(formats, limits)
    cursor = builder.cursor(config)
    if all(reached(builder, item.id, limits[item.id]) for item in formats):
        return 0, 0, set(), cursor
    if report is not None:
        report.started(config)
    accepted, effective, scanned = 0, 0, 0
    for row in rows.iter_rows(config, cursor):
        cursor = tuple(row.get("_source_cursor", cursor))
        scanned += 1
        if report is not None and scanned % SCAN_REPORT_ROWS == 0:
            report.scanned(config, scanned, effective)
        amount = _accept_row(builder, catalog, config, row, add_limits)
        if amount:
            accepted += 1
            effective += amount
            if all(reached(builder, item.id, limits[item.id]) for item in formats):
                return accepted, effective, set(), cursor
    exhausted = {
        item.id for item in formats if not reached(builder, item.id, limits[item.id])
    }
    return accepted, effective, exhausted, cursor


def _add_limits(formats, limits: Mapping[str, int]) -> dict[str, int]:
    return {
        item.id: max(limits[item.id], item.cap_bytes) if len(formats) > 1
        else limits[item.id]
        for item in formats
    }


def _accept_row(
    builder: Inventory,
    catalog: Catalog,
    config: str,
    row: dict[str, object],
    add_limits: Mapping[str, int],
) -> int:
    format_id = catalog.route(config, row)
    spec = catalog.format(format_id)
    if skip_reason(row, spec.area) is not None:
        return 0
    candidate = _candidate(format_id, row)
    if candidate is None:
        return 0
    if reached(builder, format_id, add_limits[format_id]):
        return 0
    builder.add(candidate)
    return candidate.length * candidate.weight


def reached(builder: Inventory, format_id: str, limit: int) -> bool:
    return builder.is_exhausted(format_id) or builder.capacity(format_id) >= limit


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


_SPOOL_SCHEMA = """
CREATE TABLE candidates (
    sequence INTEGER PRIMARY KEY,
    format_id TEXT NOT NULL,
    blob_id TEXT NOT NULL,
    encoding TEXT NOT NULL,
    length INTEGER NOT NULL,
    weight INTEGER NOT NULL
)
"""
