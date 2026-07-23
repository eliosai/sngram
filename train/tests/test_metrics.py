"""Distribution metrics over byte-pair count vectors."""

from __future__ import annotations

import math

import pytest

import sngram
from sngram_train import metrics


def test_kl_identical_is_zero():
    c = [3, 1, 4, 1, 5, 9, 2, 6]
    assert metrics.kl_divergence(c, c, eps=1.0) == pytest.approx(0.0, abs=1e-12)
    assert metrics.kl_divergence(c, c, eps=0.0) == pytest.approx(0.0, abs=1e-12)


def test_kl_known_value():
    # KL([.5,.5] || [.75,.25]) = .5 ln(2/3) + .5 ln 2
    expected = 0.5 * math.log(2 / 3) + 0.5 * math.log(2)
    assert metrics.kl_divergence([2, 2], [3, 1], eps=0.0) == pytest.approx(expected)


def test_kl_is_nonnegative():
    for p, q in (([5, 1, 1], [1, 1, 5]), ([10, 0, 3], [1, 7, 2])):
        assert metrics.kl_divergence(p, q, eps=1.0) >= 0.0


def test_kl_never_negative_from_roundoff_at_scale():
    # a naive sum of 65536 near-identical float terms can drift below zero
    import random

    random.seed(0)
    n = 256 * 256
    big = [10**11 + random.randint(0, 5) for _ in range(n)]
    big2 = [x + (1 if i % 2 else 0) for i, x in enumerate(big)]
    assert metrics.kl_divergence(big, big2) >= 0.0
    assert metrics.kl_divergence(big2, big) >= 0.0


def test_kl_length_mismatch_raises():
    with pytest.raises(ValueError):
        metrics.kl_divergence([1, 2], [1, 2, 3])


def test_kl_handles_unseen_pairs_without_infinity():
    # Q has a zero where P does not — eps smoothing keeps it finite
    val = metrics.kl_divergence([1, 1, 1], [1, 1, 0], eps=1.0)
    assert math.isfinite(val) and val >= 0.0


def test_counts_from_snapshot_roundtrips_counter():
    c = sngram.BigramCounter()
    c.process(b"the quick brown fox")
    counts = metrics.counts_from_snapshot(c.snapshot())
    assert len(counts) == 256 * 256
    assert counts[(ord("t") << 8) | ord("h")] == c.count(ord("t"), ord("h"))
    assert counts[(ord("z") << 8) | ord("z")] == 0


def test_kl_between_consecutive_mints_decreases():
    # cumulative snapshots of one distribution converge, so KL trends to zero
    c = sngram.BigramCounter()
    doc = b"fn main() { let x = compute(value); return x + 1; }\n"
    snaps = []
    for _ in range(4):
        for _ in range(50):
            c.process(doc)
        snaps.append(metrics.counts_from_snapshot(c.snapshot()))
    kls = [metrics.kl_divergence(snaps[i], snaps[i - 1]) for i in range(1, len(snaps))]
    assert kls[-1] < kls[0], f"KL should shrink as data accumulates: {kls}"


def test_table_frequencies_ranks_common_pairs_high():
    c = sngram.BigramCounter()
    for _ in range(200):
        c.process(b"the the the")          # 'th','he',' t' etc. very common
    c.process(b"zq")                        # 'zq' rare
    table = sngram.WeightTable.from_bytes(c.to_table_bytes())
    freqs = metrics.table_frequencies(table)
    assert len(freqs) == 256 * 256
    assert freqs[(ord("t") << 8) | ord("h")] > freqs[(ord("z") << 8) | ord("q")]
    # unseen pair contributes ~zero mass
    assert freqs[(ord("Q") << 8) | ord("Z")] == pytest.approx(0.0)


def test_table_frequencies_has_no_zeros_for_kl_safety():
    # a hard zero in Q would make KL(P||Q) infinite wherever P has mass
    c = sngram.BigramCounter()
    c.process(b"aaaa")
    table = sngram.WeightTable.from_bytes(c.to_table_bytes())
    q = metrics.table_frequencies(table)
    assert all(x > 0.0 for x in q)
    assert metrics.kl(metrics.probs_from_counts([1, 1], eps=1.0), [0.5, 0.5]) >= 0.0
