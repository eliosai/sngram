"""Exact corpus allocation across areas and formats."""

from __future__ import annotations

from collections.abc import Mapping

GB = 10**9


def apportion(total: int, weights: Mapping[str, int]) -> dict[str, int]:
    """Split an integer total with the largest-remainder method."""

    if total < 0 or not weights or any(weight < 0 for weight in weights.values()):
        raise ValueError("total and weights must be non-negative")
    denominator = sum(weights.values())
    if denominator == 0:
        raise ValueError("at least one weight must be positive")
    shares = {key: total * weight // denominator for key, weight in weights.items()}
    missing = total - sum(shares.values())
    order = sorted(weights, key=lambda key: (-(total * weights[key] % denominator), key))
    for key in order[:missing]:
        shares[key] += 1
    return dict(sorted(shares.items()))


def waterfill(total: int, capacities: Mapping[str, int | None]) -> dict[str, int]:
    """Max-min allocate total bytes across finite or unbounded formats."""

    if total < 0 or not capacities:
        raise ValueError("total must be non-negative and formats cannot be empty")
    if any(cap is not None and cap < 0 for cap in capacities.values()):
        raise ValueError("capacities must be non-negative")
    assigned = {key: 0 for key in sorted(capacities)}
    active = list(assigned)
    remaining = total
    while active:
        level = remaining // len(active)
        exhausted = [key for key in active if _below_level(capacities[key], level)]
        if not exhausted:
            _split_level(assigned, active, remaining)
            return assigned
        for key in exhausted:
            amount = int(capacities[key] or 0)
            assigned[key] = amount
            remaining -= amount
            active.remove(key)
    if remaining:
        raise ValueError("format capacity is below the requested total")
    return assigned


def _below_level(capacity: int | None, level: int) -> bool:
    return capacity is not None and capacity < level


def _split_level(assigned: dict[str, int], active: list[str], total: int) -> None:
    level, extra = divmod(total, len(active))
    for index, key in enumerate(active):
        assigned[key] = level + (index < extra)


def mint_schedule(target: int, cadence: int) -> list[int]:
    """Return bootstrap mints followed by cadence mints through target."""

    if target <= 0 or cadence <= 0:
        raise ValueError("target and cadence must be positive")
    thresholds = {value for value in (100 * GB, 500 * GB) if value < target}
    thresholds.update(range(cadence, target + 1, cadence))
    thresholds.add(target)
    return sorted(thresholds)
