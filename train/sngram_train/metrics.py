"""Distribution metrics over the 256 by 256 byte-pair table."""

from __future__ import annotations

import math
import struct

PAIR_COUNT = 256 * 256
_UNSEEN_WEIGHT = 2**32 - 1


def counts_from_snapshot(snapshot: bytes) -> list[int]:
    """Parse a counter snapshot into pair counts."""
    if len(snapshot) != PAIR_COUNT * 8:
        raise ValueError(f"snapshot must be {PAIR_COUNT * 8} bytes, got {len(snapshot)}")
    return list(struct.unpack(f"<{PAIR_COUNT}Q", snapshot))


def probs_from_counts(counts: list[int], eps: float = 1.0) -> list[float]:
    """Normalize a count vector to probabilities with add-eps smoothing."""
    total = sum(counts) + eps * len(counts)
    if total <= 0:
        return [1.0 / len(counts)] * len(counts)
    return [(c + eps) / total for c in counts]


def kl(p: list[float], q: list[float]) -> float:
    """Compute KL(P || Q) in nats."""
    if len(p) != len(q):
        raise ValueError("distributions must be the same length")
    total = 0.0
    for pi, qi in zip(p, q):
        if pi > 0.0:
            total += pi * math.log(pi / qi)
    # Clamp floating-point drift below zero
    return max(total, 0.0)


def kl_divergence(p_counts: list[int], q_counts: list[int], eps: float = 1.0) -> float:
    """Compute smoothed KL between count vectors."""
    if len(p_counts) != len(q_counts):
        raise ValueError("count vectors must be the same length")
    return kl(probs_from_counts(p_counts, eps), probs_from_counts(q_counts, eps))


def table_frequencies(table, floor: float = 1e-15) -> list[float]:
    """Recover normalized frequencies implied by a weight table."""
    inv = []
    for c1 in range(256):
        for c2 in range(256):
            w = table.weight(c1, c2)
            inv.append(floor if w >= _UNSEEN_WEIGHT else (1.0 / w) + floor)
    s = sum(inv)
    if s <= 0:
        return [1.0 / PAIR_COUNT] * PAIR_COUNT
    return [x / s for x in inv]
