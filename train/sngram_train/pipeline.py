"""Balanced durable-mint training coordinator."""

from __future__ import annotations

import time
from collections import defaultdict
from collections.abc import Mapping
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

import sngram

from . import goals as planning
from . import metrics
from .catalog import Catalog
from .checkpoint import FormatProgress, RunState, load, save, write_table
from .distribution import (
    apportion,
    mint_schedule,
    minted_baseline,
    remaining_thresholds,
    schedule_targets,
)
from .events import EventLog
from .fetching import (
    ContentReader,
    FetchPool,
    bounded_items,
    carry_estimate,
    consume,
    read_candidate,
)
from .manifest import Candidate, Manifest
from .sampling import CountSink
from .units import fmt_bytes, mint_label

FETCH_BATCH_ITEMS = 64


@dataclass(frozen=True)
class TrainerConfig:
    mint_dir: Path
    target: int
    mint_cadence: int
    workers: int
    checkpoint_interval: float
    resume: bool = True


class Trainer:
    def __init__(
        self,
        catalog: Catalog,
        manifest: Manifest,
        content: ContentReader,
        config: TrainerConfig,
        area_weights: Mapping[str, int],
        on_refresh: Callable[[Trainer], None] | None = None,
        extend: Callable[[str, int], Manifest] | None = None,
    ) -> None:
        self.catalog = catalog
        self.manifest = manifest
        self.content = content
        self.config = config
        self.area_weights = dict(area_weights)
        self.on_refresh = on_refresh
        self.extend = extend
        self._formats = {item.id: item for item in catalog.formats}
        self._area_formats = planning.formats_by_area(catalog, self.area_weights)
        self.roster_hash = manifest.roster_hash
        self._checkpoint_path = config.mint_dir / ".checkpoint.sqlite3"
        self.counter, self.state = self._load_state()
        self.committed_bytes = self.counter.bytes_processed
        self.events = EventLog(config.mint_dir / "train-events.jsonl")
        self.effective_target = min(
            config.target, manifest.effective_target or config.target
        )
        self._schedule = mint_schedule(self.effective_target, config.mint_cadence)
        self._targets = schedule_targets(self._schedule, self.area_weights)
        self._init_telemetry()

    def _init_telemetry(self) -> None:
        self.meter = metrics.RateMeter()
        self.last_checkpoint_at: float | None = None
        self.last_kl: float | None = None
        self.current_threshold: int | None = None
        self.last_goals: dict[str, int] = {}
        self.clamped = False
        self.skips = 0
        self._starved: set[str] = set()
        self._goal_cache: dict[str, int] | None = None
        self._sink = CountSink(self.counter)

    def _load_state(self) -> tuple[sngram.BigramCounter, RunState]:
        roster, revision = self.roster_hash, self.manifest.revision
        target, cadence = self.config.target, self.config.mint_cadence
        if not self.config.resume:
            return sngram.BigramCounter(), RunState(roster, revision, target, cadence)
        return load(self._checkpoint_path, roster, target, cadence, revision)

    def run(self) -> None:
        """Fill each cumulative distribution barrier and mint its table."""

        self.events.log(
            "start", target=self.effective_target, workers=self.config.workers
        )
        complete = False
        try:
            with (
                ThreadPoolExecutor(max_workers=self.config.workers) as pool,
                ThreadPoolExecutor(max_workers=2) as counters,
            ):
                self._sink.pool = counters
                reader = lambda candidate: read_candidate(self.content, candidate)
                fetch = FetchPool(pool, reader, self.config.workers * 2)
                for threshold in self._remaining_thresholds():
                    self.current_threshold = threshold
                    if not self._fill(threshold, fetch):
                        break
                    self._mint(threshold)
                    self._checkpoint()
            self._write_table("final")
            complete = True
        finally:
            self._checkpoint()
            self._log_summary(complete)
            self.events.close()

    def _log_summary(self, complete: bool) -> None:
        self.events.log(
            "summary",
            complete=complete,
            clamped=self.clamped,
            effective_bytes=self.counter.bytes_processed,
            fetched_bytes=self.fetched_bytes(),
            formats=self.format_bytes(),
            wall_s=round(time.monotonic() - self.meter.started_at, 3),
        )

    def _remaining_thresholds(self) -> list[int]:
        return remaining_thresholds(self._schedule, self.state.mints_done)

    def _fill(self, threshold: int, fetch: FetchPool) -> bool:
        targets = dict(self._targets[threshold])
        total = sum(targets.values())
        self._goal_cache = None
        while self.committed_bytes < total:
            goals = self._goal_cache
            if goals is None:
                goals = self._goals_for(targets)
            if goals is None:
                targets = self._clamped_targets(targets)
                total = sum(targets.values())
                continue
            self._goal_cache = goals
            self._pump(goals, fetch)
        return not self.clamped

    def _goals_for(self, targets: Mapping[str, int]) -> dict[str, int] | None:
        preferences = {key: item.cap_bytes for key, item in self._formats.items()}
        return planning.area_goals(
            targets, self._area_formats, preferences, self.state.progress
        )

    def _pump(self, goals: dict[str, int], fetch: FetchPool) -> None:
        self.last_goals = goals
        self._top_up(goals, fetch)
        format_id = fetch.wait_complete()
        if format_id is None:
            self._revive_starved(goals)
            return
        self._commit_format(format_id, fetch, goals[format_id])
        self._after_commit()

    def _top_up(self, goals: dict[str, int], fetch: FetchPool) -> None:
        if fetch.saturated():
            return
        for format_id in self._active(goals, fetch):
            if fetch.saturated():
                return
            items = self._plan_items(format_id, goals[format_id], fetch)
            if items is not None:
                fetch.submit(format_id, items)

    def _active(self, goals: dict[str, int], fetch: FetchPool) -> list[str]:
        progress = self.state.progress
        pending = []
        for key, goal in goals.items():
            if fetch.has_batch(key) or key in self._starved:
                continue
            item = progress(key)
            if item.exhausted or item.effective_bytes >= goal:
                continue
            pending.append((item.effective_bytes / goal, key))
        pending.sort()
        return [key for _ratio, key in pending]

    def _plan_items(
        self, format_id: str, goal: int, fetch: FetchPool
    ) -> tuple[Candidate, ...] | None:
        progress = self.state.progress(format_id)
        remaining = goal - progress.effective_bytes
        carried = fetch.carry(format_id)
        offset = 0 if carried else progress.offset
        estimate = carry_estimate(carried, progress.offset)
        if estimate >= remaining:
            return ()
        batch = self.manifest.read(
            format_id, progress.cursor + len(carried), self._batch_limit(fetch)
        )
        if not batch.items:
            if carried:
                return ()
            self._no_more_items(format_id)
            return None
        return bounded_items(batch.items, remaining - estimate, offset)

    def _batch_limit(self, fetch: FetchPool) -> int:
        share = max(self.config.workers // 4, 1)
        return max(min(share, FETCH_BATCH_ITEMS, fetch.headroom()), 1)

    def _no_more_items(self, format_id: str) -> None:
        if self.manifest.exhausted(format_id):
            self._deplete(format_id)
        else:
            self._starved.add(format_id)

    def _deplete(self, format_id: str) -> None:
        self._starved.discard(format_id)
        self._goal_cache = None
        self._set_progress(format_id, exhausted=True)
        self.events.log(
            "format_depleted", format=format_id,
            effective_bytes=self.state.progress(format_id).effective_bytes,
        )

    def _revive_starved(self, goals: dict[str, int]) -> None:
        format_id = min(self._starved, default=None)
        if format_id is None:
            return
        if self.extend is None:
            self._deplete(format_id)
            return
        before = self.manifest.candidates(format_id)
        minimum = self._extension_minimum(format_id, goals.get(format_id, 0))
        self.events.log("manifest_extend", format=format_id, minimum=minimum)
        self.manifest = self.extend(format_id, minimum)
        if self.manifest.candidates(format_id) > before:
            self._starved.discard(format_id)
        else:
            self._deplete(format_id)

    def _extension_minimum(self, format_id: str, goal: int) -> int:
        return planning.extension_minimum(
            self.manifest.capacity(format_id),
            goal,
            self.state.progress(format_id).effective_bytes,
        )

    def _commit_format(self, format_id: str, fetch: FetchPool, goal: int) -> None:
        rows = fetch.collect(format_id)
        progress = self.state.progress(format_id)
        remaining = max(goal - progress.effective_bytes, 0)
        consumed, leftover = consume(rows, remaining, progress.cursor, progress.offset)
        fetch.store_carry(format_id, leftover)
        if consumed.errors:
            self._log_skips(format_id, consumed.errors)
        effective = sum(item.length for item in consumed.slices)
        if consumed.slices:
            self._sink.submit(consumed.slices)
        self.committed_bytes += effective
        self.state.formats[format_id] = FormatProgress(
            consumed.cursor,
            consumed.offset,
            progress.effective_bytes + effective,
            progress.fetched_bytes + consumed.fetched_bytes,
            progress.objects + consumed.objects,
            progress.exhausted,
        )

    def _log_skips(self, format_id: str, errors: tuple[str, ...]) -> None:
        self.skips += len(errors)
        self.events.log(
            "content_skips", format=format_id, count=len(errors), example=errors[0]
        )

    def _after_commit(self) -> None:
        self.meter.sample(self.committed_bytes)
        last = self.last_checkpoint_at or self.meter.started_at
        if time.monotonic() - last >= self.config.checkpoint_interval:
            self._checkpoint()
        if self.on_refresh:
            self.on_refresh(self)

    def _clamped_targets(self, targets: Mapping[str, int]) -> dict[str, int]:
        baseline = minted_baseline(self._schedule, self.state.mints_done)
        base = self._targets.get(baseline, {key: 0 for key in targets})
        clamped = planning.clamped_targets(
            targets,
            base,
            self.area_weights,
            planning.area_supplies(self._area_formats, self.state.progress),
            self.area_bytes(),
        )
        if not self.clamped:
            self.clamped = True
            self.events.log(
                "target_clamped",
                requested=self.effective_target,
                achievable=sum(clamped.values()),
                areas=clamped,
            )
        return clamped

    def _mint(self, threshold: int) -> None:
        self._sink.drain()
        self._validate_barrier(threshold)
        label = mint_label(threshold)
        self._write_table(label)
        counts = self.counter.snapshot()
        if self.state.last_mint_counts is not None:
            self.last_kl = metrics.snapshot_kl(counts, self.state.last_mint_counts)
        self.state.last_mint_counts = counts
        self.state.mints_done.append(label)
        self.events.log(
            "mint",
            label=label,
            effective_bytes=self.counter.bytes_processed,
            fetched_bytes=self.fetched_bytes(),
            areas=self.area_bytes(),
            formats=self.format_bytes(),
            kl_from_prev=self.last_kl,
        )

    def _validate_barrier(self, threshold: int) -> None:
        if self.counter.bytes_processed != threshold:
            raise RuntimeError("counter did not land on the mint threshold")
        planning.validate_barrier(
            threshold,
            self.format_bytes(),
            self.area_bytes(),
            self._targets[threshold],
            self._goals_for(self._targets[threshold]),
        )

    def _set_progress(self, format_id: str, exhausted: bool) -> None:
        progress = self.state.progress(format_id)
        self.state.formats[format_id] = FormatProgress(
            progress.cursor,
            progress.offset,
            progress.effective_bytes,
            progress.fetched_bytes,
            progress.objects,
            exhausted,
        )

    def _write_table(self, label: str) -> None:
        write_table(self.config.mint_dir, label, self.counter)

    def _checkpoint(self) -> None:
        self._sink.drain()
        save(self._checkpoint_path, self.counter, self.state)
        self.last_checkpoint_at = time.monotonic()

    def format_bytes(self) -> dict[str, int]:
        progress = self.state.progress
        return {key: progress(key).effective_bytes for key in sorted(self._formats)}

    def area_bytes(self) -> dict[str, int]:
        areas: dict[str, int] = defaultdict(int)
        for format_id, amount in self.format_bytes().items():
            areas[self._formats[format_id].area] += amount
        return dict(sorted(areas.items()))

    def fetched_bytes(self) -> int:
        return sum(item.fetched_bytes for item in list(self.state.formats.values()))

    def area_targets(self, threshold: int) -> dict[str, int]:
        return dict(self._targets.get(threshold, apportion(threshold, self.area_weights)))

    def current_goals(self) -> dict[str, int]:
        if self.last_goals:
            return dict(self.last_goals)
        threshold = self.current_threshold or self.effective_target
        goals = self._goals_for(self.area_targets(threshold))
        return goals if goals is not None else self.format_bytes()

    def rate_now(self) -> float:
        return self.meter.rate_now(self.committed_bytes)

    def describe_progress(self) -> str:
        effective = fmt_bytes(self.committed_bytes)
        suffix = " (target clamped to corpus supply)" if self.clamped else ""
        return f"{effective} effective, {len(self.state.mints_done)} mints{suffix}"
