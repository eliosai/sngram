"""Structured JSONL event log + an in-memory tail for the dashboard."""

from __future__ import annotations

import json
import threading
import time
from collections import deque
from pathlib import Path

# Dashboard event kinds
TAIL_KINDS = frozenset(
    {"mint", "content_skips", "target_clamped", "format_depleted"}
)


class EventLog:
    """Append-only JSONL split into numbered segments."""

    def __init__(
        self, path: Path, tail: int = 50, segment_bytes: int = 16 * 10**6
    ) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        self._path = path
        self._segment_bytes = segment_bytes
        self._lock = threading.Lock()
        self.tail: deque[dict] = deque(maxlen=tail)
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

    @staticmethod
    def _archive_path(path: Path, seq: int) -> Path:
        """Build one numbered archive path."""
        return path.with_name(f"{path.stem}.{seq:04d}{path.suffix}")

    @classmethod
    def _archives(cls, path: Path) -> list[tuple[int, Path]]:
        """List numbered archives without sorting."""
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
        """List every segment from oldest to active."""
        archives = sorted(cls._archives(path))
        out = [p for _, p in archives]
        if path.exists():
            out.append(path)
        return out
