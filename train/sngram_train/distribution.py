"""Exact corpus allocation across areas and formats."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass

from .units import mint_label

GB = 10**9


@dataclass(frozen=True)
class Allocation:
    goals: dict[str, int]
    shortfall: int


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


def allocate(
    total: int,
    floors: Mapping[str, int],
    supplies: Mapping[str, int | None],
    preferences: Mapping[str, int | None],
) -> Allocation:
    """Max-min allocate total under soft preferences and hard supplies."""

    if total < 0 or not floors:
        raise ValueError("total must be non-negative and keys cannot be empty")
    if sum(floors.values()) > total:
        raise ValueError("floors exceed the requested total")
    soft = {key: _soft_cap(supplies[key], preferences[key], floors[key]) for key in floors}
    goals, leftover = waterlevel(total, floors, soft)
    if leftover:
        hard = {key: _hard_cap(supplies[key], floors[key]) for key in floors}
        goals, leftover = waterlevel(total, goals, hard)
    return Allocation(goals, leftover)


def waterlevel(
    total: int, floors: Mapping[str, int], caps: Mapping[str, int | None]
) -> tuple[dict[str, int], int]:
    """Raise a common level from floors to caps until total is spent."""

    if any(cap is not None and cap < floors[key] for key, cap in caps.items()):
        raise ValueError("caps must not undercut floors")
    reachable = min(total, sum(floors.values()) + _headroom(floors, caps))
    level = _level_for(reachable, floors, caps)
    goals = {key: _clamp(level, floors[key], caps[key]) for key in sorted(floors)}
    deficit = reachable - sum(goals.values())
    for key in sorted(floors):
        if deficit <= 0:
            break
        if goals[key] == level and (caps[key] is None or caps[key] > level):
            goals[key] += 1
            deficit -= 1
    return goals, total - sum(goals.values())


def _level_for(
    reachable: int, floors: Mapping[str, int], caps: Mapping[str, int | None]
) -> int:
    low, high = 0, reachable + max(floors.values(), default=0)
    while low < high:
        middle = (low + high) // 2
        filled = sum(_clamp(middle, floors[key], caps[key]) for key in floors)
        if filled < reachable:
            low = middle + 1
        else:
            high = middle
    return max(low - 1, 0)


def _clamp(level: int, floor: int, cap: int | None) -> int:
    value = max(level, floor)
    return value if cap is None else min(value, cap)


def _headroom(floors: Mapping[str, int], caps: Mapping[str, int | None]) -> int:
    infinite = 10**18
    return sum(
        infinite if cap is None else cap - floors[key] for key, cap in caps.items()
    )


def _soft_cap(supply: int | None, preference: int | None, floor: int) -> int | None:
    bounds = [bound for bound in (supply, preference) if bound is not None]
    if not bounds:
        return None
    return max(min(bounds), floor)


def _hard_cap(supply: int | None, floor: int) -> int | None:
    if supply is None:
        return None
    return max(supply, floor)


def schedule_targets(
    thresholds: Sequence[int], weights: Mapping[str, int]
) -> dict[int, dict[str, int]]:
    """Cumulative per-area targets, monotone across the mint schedule."""

    running = {key: 0 for key in weights}
    targets: dict[int, dict[str, int]] = {}
    previous = 0
    for value in thresholds:
        for key, amount in apportion(value - previous, weights).items():
            running[key] += amount
        targets[value] = dict(sorted(running.items()))
        previous = value
    return targets


def feasible_delta(limit: int, weights: Mapping[str, int], room: Mapping[str, int]) -> int:
    """Largest delta whose apportionment fits inside every finite room."""

    delta = limit
    denominator = sum(weights.values())
    for key, cap in room.items():
        if weights[key]:
            delta = min(delta, cap * denominator // weights[key])
    while delta > 0 and any(
        amount > room[key]
        for key, amount in apportion(delta, weights).items()
        if key in room
    ):
        delta -= 1
    return max(delta, 0)


def mint_schedule(target: int, cadence: int) -> list[int]:
    """Return bootstrap mints followed by cadence mints through target."""

    if target <= 0 or cadence <= 0:
        raise ValueError("target and cadence must be positive")
    thresholds = {value for value in (100 * GB, 500 * GB) if value < target}
    thresholds.update(range(cadence, target + 1, cadence))
    thresholds.add(target)
    return sorted(thresholds)


def remaining_thresholds(schedule: Sequence[int], done: Sequence[str]) -> list[int]:
    """Schedule entries whose mint labels are not finished yet."""

    finished = set(done)
    return [value for value in schedule if mint_label(value) not in finished]


def minted_baseline(schedule: Sequence[int], done: Sequence[str]) -> int:
    """Largest schedule entry already minted."""

    finished = set(done)
    return max(
        (value for value in schedule if mint_label(value) in finished), default=0
    )
