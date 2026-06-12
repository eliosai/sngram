"""Structured JSONL event log + an in-memory tail for the dashboard."""

from __future__ import annotations

import json
import threading
import time
from collections import deque
from pathlib import Path

# events worth surfacing in the dashboard's recent-events panel. checkpoint is
# deliberately absent: its status is pinned in the header, so it never crowds
# out errors/warnings/mints in the tail.
TAIL_KINDS = frozenset({"error", "warn", "mint", "stall"})


class EventLog:
    """Append-only JSONL, split into small sequential segments.

    The active file is always the base path (e.g. ``train-events.jsonl``).
    When it grows past ``segment_bytes`` it is rolled to a numbered archive
    (``train-events.0001.jsonl``, ``…0002.jsonl``, …) and a fresh active file
    is opened. Every segment is retained — a multi-day run leaves a trail of
    small, individually grep-able files instead of one unbounded log. A
    restarted run continues the numbering rather than clobbering archives.
    """

    def __init__(
        self, path: Path, tail: int = 50, segment_bytes: int = 16 * 10**6
    ) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        self._path = path
        self._segment_bytes = segment_bytes
        self._lock = threading.Lock()
        self.tail: deque[dict] = deque(maxlen=tail)
        # continue past any archives a prior run left behind
        self._seq = self._last_archive_seq(path)
        self._fh = path.open("a", encoding="utf-8")

    def log(self, kind: str, **fields: object) -> None:
        event = {"ts": round(time.time(), 3), "kind": kind, **fields}
        line = json.dumps(event, default=str)
        with self._lock:
            self._fh.write(line + "\n")
            self._fh.flush()
            if self._fh.tell() > self._segment_bytes:
                self._rotate()
            if kind in TAIL_KINDS:
                self.tail.append(event)

    def _rotate(self) -> None:
        self._fh.close()
        self._seq += 1
        self._path.replace(self._archive_path(self._path, self._seq))
        self._fh = self._path.open("a", encoding="utf-8")

    def close(self) -> None:
        with self._lock:
            self._fh.close()

    # ----------------------------------------------------------- segments

    @staticmethod
    def _archive_path(path: Path, seq: int) -> Path:
        """Numbered archive name: ``train-events.0001.jsonl``."""
        return path.with_name(f"{path.stem}.{seq:04d}{path.suffix}")

    @classmethod
    def _archives(cls, path: Path) -> list[tuple[int, Path]]:
        """(seq, file) for every numbered archive of ``path``, unsorted."""
        out: list[tuple[int, Path]] = []
        for p in path.parent.glob(f"{path.stem}.*{path.suffix}"):
            middle = p.name[len(path.stem) + 1 : -len(path.suffix)]
            if middle.isdigit():
                out.append((int(middle), p))
        return out

    @classmethod
    def _last_archive_seq(cls, path: Path) -> int:
        archives = cls._archives(path)
        return max((seq for seq, _ in archives), default=0)

    @classmethod
    def segment_paths(cls, path: Path) -> list[Path]:
        """Every segment for ``path``, oldest first: archives then the active file.

        The reader's entry point — concatenating these in order reconstructs
        the full event stream across all splits and restarts.
        """
        archives = sorted(cls._archives(path))
        out = [p for _, p in archives]
        if path.exists():
            out.append(path)
        return out
