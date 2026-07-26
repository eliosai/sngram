"""Rates, blend shares, and completion for one running trainer."""

from __future__ import annotations

import threading

from . import metrics
from .checkpoint import RunState
from .units import fmt_bytes

_TOP_BUCKETS = 8


class Progress:
    """Live rates and counted-byte shares for one run"""

    def __init__(
        self, state: RunState, lock: threading.Lock, limit: int | None, wire: int
    ) -> None:
        self.state = state
        self.decoded = metrics.RateMeter(baseline=state.decoded)
        self.wire = metrics.RateMeter(baseline=state.shard_bytes)
        self._lock = lock
        self._limit = limit
        self._wire_target = wire

    def sample(self) -> None:
        self.decoded.sample(self.state.decoded)
        self.wire.sample(self.state.shard_bytes)

    def fraction(self) -> float:
        if self._limit:
            return min(self.state.decoded / self._limit, 1.0)
        return self.state.shard_bytes / max(self._wire_target, 1)

    def rate_now(self) -> float:
        return self.decoded.rate_now(self.state.decoded)

    def rate_avg(self) -> float:
        return self.decoded.rate_avg(self.state.decoded)

    def wire_rate_now(self) -> float:
        return self.wire.rate_now(self.state.shard_bytes)

    def wire_rate_avg(self) -> float:
        return self.wire.rate_avg(self.state.shard_bytes)

    def eta_seconds(self) -> float | None:
        if self._limit:
            rate = self.rate_avg()
            remaining = max(self._limit - self.state.decoded, 0)
        else:
            rate = self.wire_rate_avg()
            remaining = max(self._wire_target - self.state.shard_bytes, 0)
        return remaining / rate if rate > 0 else None

    def mix(self, top: int = _TOP_BUCKETS) -> list[tuple[str, float]]:
        """Top shares of counted bytes, by language when the corpus reports one."""

        with self._lock:
            tallies = self.state.langs or self.state.tallies.family_bytes
            total = sum(tallies.values()) or 1
            ranked = sorted(tallies.items(), key=lambda item: -item[1])
        return [(bucket, amount / total) for bucket, amount in ranked[:top]]

    def describe(self) -> str:
        return f"{fmt_bytes(self.state.decoded)} decoded, {self.state.rows:,} rows"
