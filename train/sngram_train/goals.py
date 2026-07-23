"""Max-min goal planning over durable format progress."""

from __future__ import annotations

from collections import defaultdict
from collections.abc import Callable, Mapping

from .catalog import Catalog
from .checkpoint import FormatProgress
from .distribution import allocate, apportion, feasible_delta
from .errors import ConfigurationError

Progress = Callable[[str], FormatProgress]


def formats_by_area(
    catalog: Catalog, area_weights: Mapping[str, int]
) -> dict[str, list[str]]:
    """Group format ids by area, requiring formats for every weighted area."""

    areas: dict[str, list[str]] = defaultdict(list)
    for item in catalog.formats:
        areas[item.area].append(item.id)
    for area in area_weights:
        if not areas.get(area):
            raise ConfigurationError(f"area {area} has no formats")
    return dict(areas)


def area_goals(
    targets: Mapping[str, int],
    area_formats: Mapping[str, list[str]],
    preferences: Mapping[str, int],
    progress: Progress,
) -> dict[str, int] | None:
    """Per-format goals for the given area targets, or None on shortfall."""

    goals: dict[str, int] = {}
    for area, amount in targets.items():
        formats = area_formats[area]
        floors = {key: progress(key).effective_bytes for key in formats}
        supplies = {
            key: floors[key] if progress(key).exhausted else None for key in formats
        }
        allocation = allocate(
            amount, floors, supplies, {key: preferences[key] for key in formats}
        )
        if allocation.shortfall:
            return None
        goals.update(allocation.goals)
    return goals


def area_supplies(
    area_formats: Mapping[str, list[str]], progress: Progress
) -> dict[str, int | None]:
    """Total supply per area, finite once every format is depleted."""

    supplies: dict[str, int | None] = {}
    for area, formats in area_formats.items():
        if all(progress(key).exhausted for key in formats):
            supplies[area] = sum(progress(key).effective_bytes for key in formats)
        else:
            supplies[area] = None
    return supplies


def clamped_targets(
    targets: Mapping[str, int],
    weights: Mapping[str, int],
    supplies: Mapping[str, int | None],
    progress_by_area: Mapping[str, int],
) -> dict[str, int]:
    """Largest balanced targets that fit depleted areas, floored at progress."""

    room = {area: supply for area, supply in supplies.items() if supply is not None}
    limit = sum(targets.values())
    delta = feasible_delta(limit, weights, room)
    return {
        area: max(amount, progress_by_area.get(area, 0))
        for area, amount in apportion(delta, weights).items()
    }
