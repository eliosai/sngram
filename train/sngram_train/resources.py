"""Local resource checks for a durable training run."""

from __future__ import annotations

import shutil
from dataclasses import dataclass
from math import ceil
from pathlib import Path

from .sampling import SAMPLE_FLOOR

MANIFEST_BYTES_PER_CANDIDATE = 64
MANIFEST_RESERVE_BYTES = 5 * 10**9


@dataclass(frozen=True)
class DiskBudget:
    required_bytes: int
    free_bytes: int

    @property
    def sufficient(self) -> bool:
        return self.free_bytes >= self.required_bytes


def manifest_disk_budget(
    path: Path, target: int, extra_capacity: int = 0
) -> DiskBudget:
    """Estimate manifest space, accounting for a resumable partial file."""

    candidates = ceil((target + extra_capacity) / SAMPLE_FLOOR)
    estimate = candidates * MANIFEST_BYTES_PER_CANDIDATE
    partial = path.with_suffix(path.suffix + ".tmp")
    required = max(estimate - (partial.stat().st_size if partial.exists() else 0), 0)
    required += MANIFEST_RESERVE_BYTES
    return DiskBudget(required, shutil.disk_usage(_existing_parent(path)).free)


def _existing_parent(path: Path) -> Path:
    parent = path.parent
    while not parent.exists() and parent != parent.parent:
        parent = parent.parent
    return parent
