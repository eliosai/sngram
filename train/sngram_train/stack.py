"""Stack v2 inventory policy for the durable training manifest."""

from __future__ import annotations

import os
from collections.abc import Mapping
from pathlib import Path

from .catalog import Catalog
from .distribution import allocate, apportion, feasible_delta
from .errors import ConfigurationError
from .manifest import ManifestBuilder
from .sampling import SAMPLE_FLOOR
from .scanning import ScanReport, StackRows, reached, scan_and_commit, scan_configs


def build_stack_manifest(
    path: Path,
    catalog: Catalog,
    rows: StackRows,
    report: ScanReport | None = None,
    *,
    target: int | None = None,
    area_weights: Mapping[str, int] | None = None,
    workers: int = 1,
) -> str:
    """Build a sampled manifest covering margined adaptive format goals."""

    roster_hash = catalog.roster_hash(rows.revision)
    with ManifestBuilder(path, rows.revision, roster_hash) as builder:
        for item in catalog.formats:
            builder.register(item.id)
        if target is None:
            _static_inventory(builder, catalog, rows, report)
        else:
            if area_weights is None:
                raise ValueError("area weights are required for target-bounded inventory")
            builder.set_built_target(target)
            _adaptive_inventory(
                builder, catalog, rows, target, area_weights, report, workers
            )
    return roster_hash


def extend_manifest(
    path: Path,
    catalog: Catalog,
    rows: StackRows,
    roster_hash: str,
    format_id: str,
    minimum: int,
) -> None:
    """Grow one starved format from its stored corpus cursor."""

    temporary = path.with_suffix(path.suffix + ".tmp")
    os.replace(path, temporary)
    with ManifestBuilder(path, rows.revision, roster_hash) as builder:
        if builder.is_exhausted(format_id) or builder.capacity(format_id) >= minimum:
            return
        spec = catalog.format(format_id)
        siblings = [item for item in catalog.formats if item.config == spec.config]
        limits = {
            item.id: minimum if item.id == format_id else builder.capacity(item.id)
            for item in siblings
        }
        scan_and_commit(builder, catalog, rows, spec.config, limits, None)


def _static_inventory(
    builder: ManifestBuilder,
    catalog: Catalog,
    rows: StackRows,
    report: ScanReport | None,
) -> None:
    limits = {item.id: item.cap_bytes for item in catalog.formats}
    for config in catalog.configs:
        if builder.is_complete(config):
            continue
        scan_and_commit(builder, catalog, rows, config, limits, report)


def _adaptive_inventory(
    builder: ManifestBuilder,
    catalog: Catalog,
    rows: StackRows,
    target: int,
    area_weights: Mapping[str, int],
    report: ScanReport | None,
    workers: int,
) -> None:
    effective = target
    while True:
        goals, effective = _inventory_goals(builder, catalog, effective, area_weights)
        limits = {key: _margined(goal) for key, goal in goals.items()}
        configs = _pending_configs(builder, catalog, limits)
        if not configs:
            builder.set_effective_target(effective)
            return
        scan_configs(builder, catalog, rows, configs, limits, report, workers)


def _margined(goal: int) -> int:
    return goal + goal // 10 + 2 * SAMPLE_FLOOR


def _inventory_goals(
    builder: ManifestBuilder,
    catalog: Catalog,
    target: int,
    area_weights: Mapping[str, int],
) -> tuple[dict[str, int], int]:
    goals, rooms = _try_goals(builder, catalog, target, area_weights)
    if not rooms:
        return goals, target
    effective = feasible_delta(target, area_weights, rooms)
    goals, rooms = _try_goals(builder, catalog, effective, area_weights)
    if rooms:
        raise ConfigurationError("inventory goals did not settle after a clamp")
    return goals, effective


def _try_goals(
    builder: ManifestBuilder,
    catalog: Catalog,
    target: int,
    area_weights: Mapping[str, int],
) -> tuple[dict[str, int], dict[str, int]]:
    goals: dict[str, int] = {}
    rooms: dict[str, int] = {}
    for area, amount in apportion(target, area_weights).items():
        formats = [item for item in catalog.formats if item.area == area]
        if not formats:
            raise ConfigurationError(f"area {area} has no formats")
        supplies = {
            item.id: builder.capacity(item.id) if builder.is_exhausted(item.id) else None
            for item in formats
        }
        allocation = allocate(
            amount,
            {item.id: 0 for item in formats},
            supplies,
            {item.id: item.cap_bytes for item in formats},
        )
        goals.update(allocation.goals)
        if allocation.shortfall:
            rooms[area] = sum(value for value in supplies.values() if value is not None)
    return goals, rooms


def _pending_configs(
    builder: ManifestBuilder, catalog: Catalog, goals: Mapping[str, int]
) -> list[str]:
    return [
        config
        for config in catalog.configs
        if any(
            not reached(builder, item.id, goals[item.id])
            for item in catalog.formats
            if item.config == config
        )
    ]
