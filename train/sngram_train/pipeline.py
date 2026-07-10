"""Balanced durable-mint training coordinator."""

from __future__ import annotations

import os
import time
from collections import defaultdict
from collections.abc import Mapping
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Protocol

import sngram

from . import metrics
from .catalog import Catalog, FormatSpec
from .checkpoint import FormatProgress, RunState, load, save
from .distribution import apportion, mint_schedule, waterfill
from .events import EventLog
from .errors import CorpusExhausted
from .manifest import Candidate, Manifest
from .sampling import WeightedSlice, count_slices
from .units import fmt_bytes, mint_label


class ContentReader(Protocol):
    def read(self, blob_id: str, max_bytes: int) -> bytes: ...


@dataclass(frozen=True)
class TrainerConfig:
    mint_dir: Path
    target: int
    mint_cadence: int
    workers: int
    checkpoint_interval: float
    resume: bool = True


@dataclass(frozen=True)
class _Plan:
    format_id: str
    items: tuple[Candidate, ...]
    exhausted: bool


@dataclass(frozen=True)
class _Fetched:
    candidate: Candidate
    data: bytes | None
    fetched_bytes: int
    error: str | None = None


@dataclass(frozen=True)
class _Consumption:
    slices: tuple[WeightedSlice, ...]
    cursor: int
    offset: int
    fetched_bytes: int
    objects: int
    errors: tuple[str, ...]


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
        self.roster_hash = manifest.roster_hash
        self._checkpoint_path = config.mint_dir / ".checkpoint.sqlite3"
        self.counter, self.state = self._load_state()
        self.events = EventLog(config.mint_dir / "train-events.jsonl")
        self.started_at = time.monotonic()
        self.last_checkpoint_at: float | None = None
        self.last_kl: float | None = None
        self.current_threshold: int | None = None

    def _load_state(self) -> tuple[sngram.BigramCounter, RunState]:
        if not self.config.resume:
            return sngram.BigramCounter(), RunState(
                self.roster_hash, self.manifest.revision, self.config.target
            )
        return load(
            self._checkpoint_path,
            self.roster_hash,
            self.config.target,
            self.manifest.revision,
        )

    def run(self) -> None:
        """Fill each cumulative distribution barrier and mint its table."""

        self.events.log("start", target=self.config.target, workers=self.config.workers)
        last_checkpoint = time.monotonic()
        complete = False
        try:
            with ThreadPoolExecutor(max_workers=self.config.workers) as pool:
                for threshold in self._remaining_thresholds():
                    self.current_threshold = threshold
                    last_checkpoint = self._fill(threshold, pool, last_checkpoint)
                    self._mint(threshold)
                    self._checkpoint()
            self._write_table("final")
            complete = True
        finally:
            self._checkpoint()
            self.events.log(
                "summary",
                complete=complete,
                effective_bytes=self.counter.bytes_processed,
                fetched_bytes=self.fetched_bytes(),
                formats=self.format_bytes(),
                wall_s=round(time.monotonic() - self.started_at, 3),
            )
            self.events.close()

    def _remaining_thresholds(self) -> list[int]:
        done = set(self.state.mints_done)
        return [
            value
            for value in mint_schedule(self.config.target, self.config.mint_cadence)
            if mint_label(value) not in done
        ]

    def _fill(
        self, threshold: int, pool: ThreadPoolExecutor, last_checkpoint: float
    ) -> float:
        while self.counter.bytes_processed < threshold:
            goals = self._format_goals(threshold)
            plans = self._plans(goals)
            if not plans:
                raise CorpusExhausted(
                    f"corpus exhausted before balanced {threshold} byte mint"
                )
            fetched = self._fetch(plans, pool)
            self._commit(plans, fetched, goals)
            if time.monotonic() - last_checkpoint >= self.config.checkpoint_interval:
                self._checkpoint()
                last_checkpoint = time.monotonic()
            if self.on_refresh:
                self.on_refresh(self)
        return last_checkpoint

    def _format_goals(self, threshold: int) -> dict[str, int]:
        area_targets = apportion(threshold, self.area_weights)
        goals: dict[str, int] = {}
        for area, target in area_targets.items():
            formats = [item for item in self.catalog.formats if item.area == area]
            if not formats:
                raise CorpusExhausted(f"area {area} has no formats")
            capacities = {item.id: self._capacity(item) for item in formats}
            try:
                goals.update(waterfill(target, capacities))
            except ValueError as error:
                raise CorpusExhausted(
                    f"area {area} exhausted below {target} bytes"
                ) from error
        return goals

    def _capacity(self, spec: FormatSpec) -> int:
        progress = self.state.progress(spec.id)
        if progress.exhausted:
            return progress.effective_bytes
        return min(spec.cap_bytes, self.manifest.capacity(spec.id))

    def _plans(self, goals: Mapping[str, int]) -> list[_Plan]:
        active = [
            key
            for key, goal in goals.items()
            if self.state.progress(key).effective_bytes < goal
            and not self.state.progress(key).exhausted
        ]
        active.sort(key=lambda key: (self.state.progress(key).effective_bytes / goals[key], key))
        slots = max(self.config.workers, 1)
        per_format = max(slots // max(len(active), 1), 1)
        plans = []
        for format_id in active:
            if slots <= 0:
                break
            plan = self._plan_format(format_id, goals[format_id], min(per_format, slots))
            if plan is not None:
                plans.append(plan)
                slots -= len(plan.items)
        return plans

    def _plan_format(self, format_id: str, goal: int, limit: int) -> _Plan | None:
        progress = self.state.progress(format_id)
        batch = self.manifest.read(format_id, progress.cursor, 1 if progress.offset else limit)
        if not batch.items:
            self._set_progress(format_id, exhausted=True)
            return None
        remaining = goal - progress.effective_bytes
        items = _bounded_items(batch.items, remaining, progress.offset)
        exhausted = batch.exhausted and len(items) == len(batch.items)
        return _Plan(format_id, items, exhausted)

    def _fetch(self, plans: list[_Plan], pool: ThreadPoolExecutor) -> dict[str, list[_Fetched]]:
        items = [(plan.format_id, item) for plan in plans for item in plan.items]
        futures = [(format_id, pool.submit(self._read, item)) for format_id, item in items]
        fetched: dict[str, list[_Fetched]] = defaultdict(list)
        for format_id, future in futures:
            fetched[format_id].append(future.result())
        return fetched

    def _read(self, candidate: Candidate) -> _Fetched:
        try:
            raw = self.content.read(candidate.blob_id, candidate.length)
            data = raw.decode(candidate.encoding).encode("utf-8")
            if not data:
                raise ValueError("decoded content is empty")
            return _Fetched(candidate, data, len(raw))
        except (FileNotFoundError, LookupError, UnicodeError, ValueError) as error:
            return _Fetched(candidate, None, 0, str(error)[:300])

    def _commit(
        self,
        plans: list[_Plan],
        fetched: dict[str, list[_Fetched]],
        goals: Mapping[str, int],
    ) -> None:
        for plan in plans:
            self._commit_format(plan, fetched[plan.format_id], goals[plan.format_id])

    def _commit_format(self, plan: _Plan, rows: list[_Fetched], goal: int) -> None:
        progress = self.state.progress(plan.format_id)
        remaining = goal - progress.effective_bytes
        consumed = _consume(rows, remaining, progress.cursor, progress.offset)
        if consumed.errors:
            self.events.log(
                "content_skips",
                format=plan.format_id,
                count=len(consumed.errors),
                example=consumed.errors[0],
            )
        counted = count_slices(consumed.slices) if consumed.slices else None
        if counted:
            self.counter.merge(counted.counter)
        effective = counted.effective_bytes if counted else 0
        exhausted = plan.exhausted and consumed.cursor >= progress.cursor + len(plan.items)
        self.state.formats[plan.format_id] = FormatProgress(
            consumed.cursor,
            consumed.offset,
            progress.effective_bytes + effective,
            progress.fetched_bytes + consumed.fetched_bytes,
            progress.objects + consumed.objects,
            exhausted,
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

    def _mint(self, threshold: int) -> None:
        self._validate_barrier(threshold)
        label = mint_label(threshold)
        self._write_table(label)
        counts = self.counter.snapshot()
        if self.state.last_mint_counts is not None:
            current = metrics.counts_from_snapshot(counts)
            previous = metrics.counts_from_snapshot(self.state.last_mint_counts)
            self.last_kl = metrics.kl_divergence(current, previous)
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
        if sum(self.format_bytes().values()) != threshold:
            raise RuntimeError("format progress does not match the counter")
        if self.area_bytes() != apportion(threshold, self.area_weights):
            raise RuntimeError("area progress does not match the mint contract")
        goals = self._format_goals(threshold)
        if any(self.state.progress(key).effective_bytes != goal for key, goal in goals.items()):
            raise RuntimeError("format progress does not match the mint contract")

    def _write_table(self, label: str) -> None:
        self.config.mint_dir.mkdir(parents=True, exist_ok=True)
        path = self.config.mint_dir / f"{label}_weights.bin"
        temporary = path.with_suffix(".bin.tmp")
        temporary.write_bytes(self.counter.to_table_bytes())
        os.replace(temporary, path)

    def _checkpoint(self) -> None:
        save(self._checkpoint_path, self.counter, self.state)
        self.last_checkpoint_at = time.monotonic()

    def format_bytes(self) -> dict[str, int]:
        return {
            key: self.state.progress(key).effective_bytes for key in sorted(self._formats)
        }

    def area_bytes(self) -> dict[str, int]:
        areas: dict[str, int] = defaultdict(int)
        for format_id, amount in self.format_bytes().items():
            areas[self._formats[format_id].area] += amount
        return dict(sorted(areas.items()))

    def fetched_bytes(self) -> int:
        return sum(progress.fetched_bytes for progress in self.state.formats.values())

    def current_goals(self) -> dict[str, int]:
        return self._format_goals(self.current_threshold or self.config.target)

    def rate_avg(self) -> float:
        elapsed = max(time.monotonic() - self.started_at, 1e-6)
        return self.counter.bytes_processed / elapsed

    def describe_progress(self) -> str:
        effective = fmt_bytes(self.counter.bytes_processed)
        return f"{effective} effective, {len(self.state.mints_done)} mints"


def _bounded_items(
    items: tuple[Candidate, ...], remaining: int, offset: int
) -> tuple[Candidate, ...]:
    selected = []
    estimated = -offset
    for item in items:
        selected.append(item)
        estimated += item.length * item.weight
        if estimated >= remaining:
            break
    return tuple(selected)


def _consume(
    rows: list[_Fetched], remaining: int, cursor: int, offset: int
) -> _Consumption:
    slices: list[WeightedSlice] = []
    errors: list[str] = []
    fetched_bytes = 0
    objects = 0
    for row in rows:
        fetched_bytes += row.fetched_bytes
        if row.data is None:
            cursor += 1
            offset = 0
            errors.append(row.error or "content read failed")
            continue
        objects += 1
        available = len(row.data) * row.candidate.weight - offset
        taken = min(available, remaining)
        slices.append(WeightedSlice(row.data, row.candidate.weight, offset, taken))
        remaining -= taken
        if taken < available:
            offset += taken
            break
        cursor += 1
        offset = 0
        if remaining == 0:
            break
    return _Consumption(
        tuple(slices),
        cursor,
        offset,
        fetched_bytes,
        objects,
        tuple(errors),
    )
