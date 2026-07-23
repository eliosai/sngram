"""Balanced durable training coordinator."""

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
from .distribution import apportion
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
from .units import fmt_bytes

FETCH_BATCH_ITEMS = 64


@dataclass(frozen=True)
class TrainerConfig:
    mint_dir: Path
    target: int
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
    ) -> None:
        self.catalog = catalog
        self.manifest = manifest
        self.content = content
        self.config = config
        self.area_weights = dict(area_weights)
        self.on_refresh = on_refresh
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
        self._targets = apportion(self.effective_target, self.area_weights)
        self._init_telemetry()

    def _init_telemetry(self) -> None:
        self.meter = metrics.RateMeter()
        self.last_checkpoint_at: float | None = None
        self.last_goals: dict[str, int] = {}
        self.clamped = False
        self.skips = 0
        self._starved: set[str] = set()
        self._goal_cache: dict[str, int] | None = None
        self._sink = CountSink(self.counter)

    def _load_state(self) -> tuple[sngram.BigramCounter, RunState]:
        roster, revision = self.roster_hash, self.manifest.revision
        if not self.config.resume:
            return sngram.BigramCounter(), RunState(roster, revision, self.config.target)
        return load(self._checkpoint_path, roster, self.config.target, revision)

    def run(self) -> None:
        """Fill the balanced distribution and mint the final table."""

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
                self._fill(fetch)
            self._mint_final()
            complete = True
        finally:
            self._checkpoint()
            self._log_summary(complete)
            self.events.close()

    def _fill(self, fetch: FetchPool) -> None:
        targets = dict(self._targets)
        total = sum(targets.values())
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
            self._deplete_starved()
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
            self._deplete(format_id)
            return None
        return bounded_items(batch.items, remaining - estimate, offset)

    def _batch_limit(self, fetch: FetchPool) -> int:
        share = max(self.config.workers // 4, 1)
        return max(min(share, FETCH_BATCH_ITEMS, fetch.headroom()), 1)

    def _deplete(self, format_id: str) -> None:
        self._starved.discard(format_id)
        self._goal_cache = None
        self._set_progress(format_id, exhausted=True)
        self.events.log(
            "format_depleted", format=format_id,
            effective_bytes=self.state.progress(format_id).effective_bytes,
        )

    def _deplete_starved(self) -> None:
        format_id = min(self._starved, default=None)
        if format_id is not None:
            self._deplete(format_id)

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
        clamped = planning.clamped_targets(
            targets,
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

    def _mint_final(self) -> None:
        self._sink.drain()
        if self.counter.bytes_processed != self.committed_bytes:
            raise RuntimeError("counter does not match committed progress")
        write_table(self.config.mint_dir, "final", self.counter, self._provenance())
        self.events.log(
            "mint",
            label="final",
            effective_bytes=self.counter.bytes_processed,
            fetched_bytes=self.fetched_bytes(),
            areas=self.area_bytes(),
            formats=self.format_bytes(),
        )

    def _provenance(self) -> str:
        return (
            f"sngram-train stack-v2@{self.manifest.revision[:12]} "
            f"{self.counter.bytes_processed} effective bytes "
            f"{self.counter.files_processed} objects"
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

    def _checkpoint(self) -> None:
        self._sink.drain()
        save(self._checkpoint_path, self.counter, self.state)
        self.last_checkpoint_at = time.monotonic()
        self.events.log(
            "progress",
            effective_bytes=self.committed_bytes,
            fetched_bytes=self.fetched_bytes(),
            rate=round(self.rate_now(), 1),
        )

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

    def area_targets(self) -> dict[str, int]:
        return dict(self._targets)

    def current_goals(self) -> dict[str, int]:
        if self.last_goals:
            return dict(self.last_goals)
        goals = self._goals_for(self._targets)
        return goals if goals is not None else self.format_bytes()

    def rate_now(self) -> float:
        return self.meter.rate_now(self.committed_bytes)

    def describe_progress(self) -> str:
        effective = fmt_bytes(self.committed_bytes)
        suffix = " (target clamped to corpus supply)" if self.clamped else ""
        return f"{effective} effective{suffix}"
