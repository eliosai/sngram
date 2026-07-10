"""Validate a minted table against searchable filesystem text."""

from __future__ import annotations

import math
import os
from collections.abc import Iterable, Iterator
from dataclasses import dataclass, field

import sngram

from . import metrics

HEAD_BYTES = 8192
DEFAULT_MAX_FILE = 20_000_000


def is_binary(head: bytes) -> bool:
    """Treat a file with a NUL in its head as binary."""
    return b"\x00" in head


def byte_pair_counts(chunks: Iterable[bytes]) -> list[int]:
    """Count byte pairs without crossing document boundaries."""
    counter = sngram.BigramCounter()
    for chunk in chunks:
        counter.process(chunk)
    return metrics.counts_from_snapshot(counter.snapshot())


@dataclass
class FsStats:
    files: int = 0
    skipped_binary: int = 0
    skipped_other: int = 0
    total_bytes: int = 0
    ext_bytes: dict[str, int] = field(default_factory=dict)


def _walk_files(roots: Iterable[str]) -> Iterator[str]:
    for root in roots:
        if os.path.isfile(root):
            yield root
            continue
        for dirpath, _dirs, names in os.walk(root):
            for name in names:
                yield os.path.join(dirpath, name)


def filesystem_histogram(
    roots: Iterable[str],
    *,
    max_file: int = DEFAULT_MAX_FILE,
    head_bytes: int = HEAD_BYTES,
    cap: int | None = None,
) -> tuple[list[int], FsStats]:
    """Count searchable files under roots up to an optional cap."""
    counter = sngram.BigramCounter()
    stats = FsStats()
    for path in _walk_files(roots):
        if cap is not None and stats.total_bytes >= cap:
            break
        try:
            if os.path.islink(path) or not os.path.isfile(path):
                continue
            size = os.path.getsize(path)
            if size == 0 or size > max_file:
                stats.skipped_other += 1
                continue
            with open(path, "rb") as fh:
                head = fh.read(head_bytes)
                if is_binary(head):
                    stats.skipped_binary += 1
                    continue
                data = head + fh.read(max_file - len(head))
        except OSError:
            stats.skipped_other += 1
            continue
        if not data:
            continue
        counter.process(data)
        stats.files += 1
        stats.total_bytes += len(data)
        ext = os.path.splitext(path)[1].lower() or "(noext)"
        stats.ext_bytes[ext] = stats.ext_bytes.get(ext, 0) + len(data)
    return metrics.counts_from_snapshot(counter.snapshot()), stats


def _pair(index: int) -> tuple[int, int]:
    return index >> 8, index & 0xFF


@dataclass
class ValidationReport:
    kl: float
    under_weighted: list[tuple[tuple[int, int], float, float, float]]
    over_weighted: list[tuple[tuple[int, int], float, float, float]]


def validate(fs_counts: list[int], table, top: int = 20) -> ValidationReport:
    """Compare filesystem counts with a table using KL contribution."""
    p = metrics.probs_from_counts(fs_counts, eps=1.0)
    q = metrics.table_frequencies(table)
    kl = metrics.kl(p, q)
    under: list[tuple[tuple[int, int], float, float, float]] = []
    over: list[tuple[tuple[int, int], float, float, float]] = []
    for i in range(len(p)):
        pi, qi = p[i], q[i]
        if pi > qi:
            under.append((_pair(i), pi, qi, pi * math.log(pi / qi)))
        elif qi > pi:
            over.append((_pair(i), pi, qi, qi * math.log(qi / pi)))
    under.sort(key=lambda d: d[3], reverse=True)
    over.sort(key=lambda d: d[3], reverse=True)
    return ValidationReport(kl=kl, under_weighted=under[:top], over_weighted=over[:top])
