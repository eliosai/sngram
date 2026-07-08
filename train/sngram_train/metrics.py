"""Distribution metrics over the 256x256 byte-pair table.

Two uses:
- **convergence**: KL(mint_n || mint_{n-1}) between consecutive mints' count
  vectors — once it stops shrinking, more data buys nothing and the run can stop.
- **ideal-distribution check**: KL between a minted table's implied frequencies
  and a real filesystem's byte-pair histogram (see fsvalidate), so the corpus
  mix can be tuned to match what regex search actually runs over.

All in nats. Add-eps (Laplace) smoothing keeps unseen pairs finite.
"""

from __future__ import annotations

import math
import struct

PAIR_COUNT = 256 * 256
_UNSEEN_WEIGHT = 2**32 - 1  # u32::MAX, the weight a never-seen pair gets


def counts_from_snapshot(snapshot: bytes) -> list[int]:
    """Parse a `BigramCounter.snapshot()` blob (65,536 little-endian u64) into a
    flat list of per-pair counts, indexed `(c1 << 8) | c2`."""
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
    """KL(P || Q) in nats for two probability vectors. Both must be strictly
    positive (smooth first) and the same length."""
    if len(p) != len(q):
        raise ValueError("distributions must be the same length")
    total = 0.0
    for pi, qi in zip(p, q):
        if pi > 0.0:
            total += pi * math.log(pi / qi)
    # KL >= 0 is a mathematical invariant; summing 65536 float terms for two
    # near-identical mints can underflow a hair below 0. Clamp so the
    # convergence/early-stop signal never reports a negative divergence.
    return max(total, 0.0)


def kl_divergence(p_counts: list[int], q_counts: list[int], eps: float = 1.0) -> float:
    """KL(P || Q) between two count vectors, with add-eps smoothing so a pair
    unseen in Q never produces an infinity. Identical vectors give exactly 0."""
    if len(p_counts) != len(q_counts):
        raise ValueError("count vectors must be the same length")
    return kl(probs_from_counts(p_counts, eps), probs_from_counts(q_counts, eps))


def table_frequencies(table, floor: float = 1e-15) -> list[float]:
    """Recover the byte-pair frequency distribution implied by a `WeightTable`.

    weight = total_pairs / count, so count ∝ 1/weight and the frequencies are
    1/weight renormalized; an unseen pair (weight u32::MAX) contributes ~zero.
    The small default `floor` keeps every entry strictly positive so the result
    is safe as the Q of a KL even where P has mass on a pair the table never saw
    (a hard zero would make KL silently +inf). Pass floor=0.0 for exact zeros.

    NOTE: weight is u32 = total // count (integer division, learn.rs), so for the
    very highest-mass pairs (count huge -> weight 1-2) the reconstructed
    frequency is coarsely quantized and can be off by a few percentage points.
    fs-validate's absolute KL is therefore approximate for the commonest pairs;
    the contribution-ranked over/under-weighted lists remain reliable (they key
    on mid-frequency mismatches, where weights are large and quantization small).
    """
    inv = []
    for c1 in range(256):
        for c2 in range(256):
            w = table.weight(c1, c2)
            inv.append(floor if w >= _UNSEEN_WEIGHT else (1.0 / w) + floor)
    s = sum(inv)
    if s <= 0:
        return [1.0 / PAIR_COUNT] * PAIR_COUNT
    return [x / s for x in inv]
