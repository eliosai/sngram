"""Structured JSONL event log + an in-memory tail for the dashboard."""

from __future__ import annotations

import json
import threading
import time
from collections import deque
from pathlib import Path


class EventLog:
    """Append-only JSONL: one machine-readable line per pipeline event.

    Rotates at `max_bytes` (keeping one .1 predecessor) so a multi-day run
    cannot fill the disk with its own log.
    """

    def __init__(self, path: Path, tail: int = 50, max_bytes: int = 256 * 10**6) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        self._path = path
        self._max_bytes = max_bytes
        self._fh = path.open("a", encoding="utf-8")
        self._lock = threading.Lock()
        self.tail: deque[dict] = deque(maxlen=tail)

    def log(self, kind: str, **fields: object) -> None:
        event = {"ts": round(time.time(), 3), "kind": kind, **fields}
        line = json.dumps(event, default=str)
        with self._lock:
            self._fh.write(line + "\n")
            self._fh.flush()
            if self._fh.tell() > self._max_bytes:
                self._rotate()
            if kind in {"error", "warn", "mint", "checkpoint", "stall"}:
                self.tail.append(event)

    def _rotate(self) -> None:
        self._fh.close()
        self._path.replace(self._path.with_suffix(self._path.suffix + ".1"))
        self._fh = self._path.open("a", encoding="utf-8")

    def close(self) -> None:
        with self._lock:
            self._fh.close()
