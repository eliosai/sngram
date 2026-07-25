"""Batch boundary pause gate for consistent checkpoints."""

from __future__ import annotations

import threading


class Gate:
    """Parks every worker at its next batch boundary while state is saved."""

    def __init__(self) -> None:
        self._cond = threading.Condition()
        self._pausing = False
        self._generation = 0
        self._active = 0
        self._waiting = 0

    def enter(self) -> None:
        with self._cond:
            self._active += 1

    def leave(self) -> None:
        with self._cond:
            self._active -= 1
            self._cond.notify_all()

    def pause_point(self) -> None:
        """Block here while a checkpoint holds the run."""

        with self._cond:
            if not self._pausing:
                return
            self._waiting += 1
            self._cond.notify_all()
            generation = self._generation
            self._cond.wait_for(lambda: self._generation != generation)

    def hold(self) -> None:
        """Wait until every active worker is parked at its boundary."""

        with self._cond:
            self._pausing = True
            self._cond.wait_for(lambda: self._waiting >= self._active)

    def release(self) -> None:
        with self._cond:
            self._pausing = False
            self._waiting = 0
            self._generation += 1
            self._cond.notify_all()
