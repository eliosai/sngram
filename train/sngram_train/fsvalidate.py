"""Validate a minted weight table against a real filesystem.

The ideal byte-pair distribution for regex search over a Linux filesystem is the
distribution of the files you actually grep. So measure it directly: walk real
roots, skip binary files (as ripgrep/git do — a NUL byte in the head), and
histogram the byte pairs of the text files. KL-divergence between that histogram
and a minted table's implied distribution scores how well the corpus mix matches
reality; a per-pair diff shows which pairs the corpus over- or under-weights, so
the mix weights can be tuned toward the measured target.

Skipping whole binary files is corpus *definition*, not byte filtering: those
files are never searched, so they are not part of the distribution. Within a
text file every byte is counted, odd bytes included.
"""

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
    """Treat a file as binary (not searched) if its head holds a NUL — the same
    cheap heuristic git and ripgrep use to skip non-text files."""
    return b"\x00" in head


def byte_pair_counts(chunks: Iterable[bytes]) -> list[int]:
    """Byte-pair counts over a sequence of documents, per document (no pair
    straddles a document boundary), indexed `(c1 << 8) | c2`."""
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
    """Byte-pair counts of the searchable (text) files under `roots`, plus stats.

    Symlinks, empty/oversized files, and binaries (NUL in the head) are skipped.
    `cap` bounds the total bytes read, for sampling a huge tree.
    """
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
    # (pair, fs_freq, table_freq, divergence_contribution); under = fs has more
    # than the corpus produced (corpus under-represents); over = the reverse
    under_weighted: list[tuple[tuple[int, int], float, float, float]]
    over_weighted: list[tuple[tuple[int, int], float, float, float]]


def validate(fs_counts: list[int], table, top: int = 20) -> ValidationReport:
    """Score a minted table against a filesystem byte-pair histogram.

    KL(filesystem || table) summarizes the mismatch. Pairs are ranked by their
    *contribution to the divergence* — `p·log(p/q)` for under-represented pairs,
    `q·log(q/p)` for over-represented ones — NOT by raw log-ratio. Raw log-ratio
    is dominated by pairs the table never saw (q at the floor), which surface
    rare, single-occurrence noise as if it were actionable; weighting by the
    frequency on the side that has the mass ranks the pairs that actually move
    the table first.
    """
    p = metrics.probs_from_counts(fs_counts, eps=1.0)
    q = metrics.table_frequencies(table)  # KL-safe: strictly positive
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
