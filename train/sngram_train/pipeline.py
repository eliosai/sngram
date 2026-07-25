"""Parallel stack-v3 trainer."""

from __future__ import annotations

import gc
import shutil
import threading
import time
from collections import deque
from dataclasses import dataclass
from pathlib import Path

import sngram

from . import metrics
from .checkpoint import RunState, load, save, write_table
from .corpus import Corpus
from .errors import is_transient
from .events import EventLog
from .gate import Gate
from .reader import ShardReader, Sifted

_RETRY_LIMIT = 6
_RETRY_BASE = 2.0
_JOIN_TIMEOUT = 120.0
_TOP_LANGS = 8
_SPOOL_CHUNK = 8 * 2**20
_DECODE_SLOTS = 1
_GC_PERIOD = 2.0


@dataclass(frozen=True)
class TrainerConfig:
    mint_dir: Path
    workers: int
    checkpoint_interval: float
    limit: int | None = None
    resume: bool = True


class Trainer:
    """Spools shards to disk in parallel and decodes them into one counter."""

    def __init__(self, corpus: Corpus, source, config: TrainerConfig) -> None:
        self.corpus = corpus
        self.source = source
        self.config = config
        self._checkpoint_path = config.mint_dir / ".checkpoint.sqlite3"
        self.counter, self.state = self._load_state()
        self.events = EventLog(config.mint_dir / "train-events.jsonl")
        self.lock = threading.Lock()
        self.gate = Gate()
        self.stop = threading.Event()
        self._slots = threading.Semaphore(_DECODE_SLOTS)
        self._spool_dir = config.mint_dir / ".spool"
        self.failure: BaseException | None = None
        self.retries = 0
        self.wire_target = corpus.wire_bytes()
        self.meter = metrics.RateMeter(baseline=self.state.decoded)
        self.wire_meter = metrics.RateMeter(baseline=self.state.shard_bytes)
        self.last_checkpoint_at: float | None = None
        self._cursor, self._resumable, self._partial = self._positions()

    def _load_state(self) -> tuple[sngram.BigramCounter, RunState]:
        if not self.config.resume:
            return sngram.BigramCounter(), RunState(self.corpus.revision, self.corpus.repo)
        return load(self._checkpoint_path, self.corpus.revision, self.corpus.repo)

    def _positions(self) -> tuple[int, deque[int], dict[int, tuple[int, int]]]:
        saved = self.state.stream_state or {}
        partial = {
            int(shard): (int(group), int(rows))
            for shard, group, rows in saved.get("partial", [])
        }
        return saved.get("cursor", 0), deque(sorted(partial)), partial

    def run(self) -> None:
        """Stream the corpus through the counter and mint the final table."""

        shutil.rmtree(self._spool_dir, ignore_errors=True)
        self._spool_dir.mkdir(parents=True, exist_ok=True)
        self.events.log(
            "start",
            shards=len(self.corpus.shards),
            wire_bytes=self.wire_target,
            workers=self.config.workers,
            limit=self.config.limit,
        )
        complete = False
        try:
            self._train()
            self._mint_final()
            complete = True
        finally:
            self._log_summary(complete)
            self.events.close()

    def _train(self) -> None:
        threads = [
            threading.Thread(target=self._worker, name=f"shard-{index}", daemon=True)
            for index in range(self.config.workers)
        ]
        for thread in threads:
            thread.start()
        try:
            self._supervise(threads)
        finally:
            self.stop.set()
            for thread in threads:
                thread.join(timeout=_JOIN_TIMEOUT)
            if any(thread.is_alive() for thread in threads):
                self.events.log("error", error="worker hung, checkpoint skipped")
            else:
                self._checkpoint()
                shutil.rmtree(self._spool_dir, ignore_errors=True)
        if self.failure is not None:
            raise self.failure

    def _supervise(self, threads: list[threading.Thread]) -> None:
        last_sweep = time.monotonic()
        while any(thread.is_alive() for thread in threads):
            time.sleep(0.2)
            self.meter.sample(self.state.decoded)
            self.wire_meter.sample(self.state.shard_bytes)
            if time.monotonic() - last_sweep >= _GC_PERIOD:
                gc.collect()
                last_sweep = time.monotonic()
            if self.config.limit and self.state.decoded >= self.config.limit:
                self.stop.set()
            elif self._checkpoint_due():
                self._checkpoint()

    def _checkpoint_due(self) -> bool:
        last = self.last_checkpoint_at or self.meter.started_at
        return time.monotonic() - last >= self.config.checkpoint_interval

    def _checkpoint(self) -> None:
        try:
            self.gate.hold()
            self.state.stream_state = self._stream_state()
            save(self._checkpoint_path, self.counter, self.state)
        finally:
            self.gate.release()
        self.last_checkpoint_at = time.monotonic()
        self.events.log(
            "progress",
            decoded=self.state.decoded,
            shard_bytes=self.state.shard_bytes,
            repos=self.state.repos,
            rate=round(self.rate_now(), 1),
            mix={language: round(share, 4) for language, share in self.language_mix()},
        )

    def _stream_state(self) -> dict:
        with self.lock:
            partial = [
                [shard, group, rows]
                for shard, (group, rows) in sorted(self._partial.items())
            ]
            return {"cursor": self._cursor, "partial": partial}

    def _worker(self) -> None:
        self.gate.enter()
        try:
            while not self.stop.is_set():
                unit = self._next_unit()
                if unit is None:
                    return
                self._consume(unit)
        except BaseException as error:
            with self.lock:
                if self.failure is None:
                    self.failure = error
            self.stop.set()
        finally:
            self.gate.leave()

    def _next_unit(self) -> tuple[int, int, int] | None:
        with self.lock:
            if self._resumable:
                shard = self._resumable.popleft()
                group, rows = self._partial[shard]
                return shard, group, rows
            if self._cursor >= len(self.corpus.shards):
                return None
            shard = self._cursor
            self._cursor += 1
            self._partial[shard] = (0, 0)
            return shard, 0, 0

    def _consume(self, unit: tuple[int, int, int]) -> None:
        shard = unit[0]
        attempts = 0
        while not self.stop.is_set():
            try:
                self._read_shard(shard, unit[1], unit[2])
                return
            except Exception as error:
                if not is_transient(error) or attempts >= _RETRY_LIMIT:
                    raise
                attempts += 1
                self._backoff(shard, error, attempts)
                with self.lock:
                    unit = (shard, *self._partial[shard])

    def _backoff(self, shard: int, error: Exception, attempt: int) -> None:
        with self.lock:
            self.retries += 1
        self.events.log(
            "retry", shard=shard, attempt=attempt, error=str(error)[:200]
        )
        deadline = time.monotonic() + min(_RETRY_BASE * 2 ** (attempt - 1), 60.0)
        while time.monotonic() < deadline and not self.stop.is_set():
            self.gate.pause_point()
            time.sleep(0.2)

    def _read_shard(self, shard: int, start_group: int, start_rows: int) -> None:
        path = self._spool(shard)
        if path is None:
            return
        try:
            with open(path, "rb") as handle:
                reader = ShardReader(handle)
                for group in range(start_group, reader.row_groups):
                    skip = start_rows if group == start_group else 0
                    if not self._read_group(reader, shard, group, skip):
                        return
        finally:
            path.unlink(missing_ok=True)
        with self.lock:
            del self._partial[shard]

    def _spool(self, shard: int) -> Path | None:
        path = self._spool_dir / f"shard-{shard:05d}.parquet"
        with self.source.open(self.corpus.shards[shard].name) as remote:
            with open(path, "wb") as local:
                while chunk := remote.read(_SPOOL_CHUNK):
                    local.write(chunk)
                    self.gate.pause_point()
                    if self.stop.is_set():
                        path.unlink(missing_ok=True)
                        return None
        return path

    def _acquire_slot(self) -> bool:
        while not self.stop.is_set():
            if self._slots.acquire(timeout=0.1):
                return True
            self.gate.pause_point()
        return False

    def _read_group(self, reader: ShardReader, shard: int, group: int, skip: int) -> bool:
        if not self._acquire_slot():
            return False
        batches = reader.batches(group, skip)
        try:
            rows = skip
            for sifted in batches:
                rows += sifted.repos
                self._commit(sifted, shard, group, rows)
                del sifted
                self.gate.pause_point()
                if self.stop.is_set():
                    return False
        finally:
            batches.close()
            self._slots.release()
        self._advance_shard(shard, group, reader.row_groups)
        return True

    def _commit(self, sifted: Sifted, shard: int, group: int, rows: int) -> None:
        staged = sngram.BigramCounter()
        staged.count_arrow(sifted.content)
        staged.add_files(sifted.files)
        self.counter.merge(staged)
        with self.lock:
            self.state.decoded += staged.bytes_processed
            self.state.repos += sifted.repos
            self.state.vendor_files += sifted.vendor_files
            for language, amount in sifted.lang_bytes.items():
                self.state.langs[language] = self.state.langs.get(language, 0) + amount
            self._partial[shard] = (group, rows)

    def _advance_shard(self, shard: int, group: int, groups: int) -> None:
        size = self.corpus.shards[shard].size
        share = size // groups
        if group == groups - 1:
            share = size - share * (groups - 1)
        with self.lock:
            self._partial[shard] = (group + 1, 0)
            self.state.shard_bytes += share

    def _mint_final(self) -> None:
        if self.counter.bytes_processed != self.state.decoded:
            raise RuntimeError("counter does not match committed progress")
        write_table(self.config.mint_dir, "final", self.counter, self._provenance())
        self.events.log(
            "mint",
            label="final",
            decoded=self.state.decoded,
            repos=self.state.repos,
            files=self.counter.files_processed,
            vendor_files=self.state.vendor_files,
            languages=dict(sorted(self.state.langs.items())),
        )

    def _provenance(self) -> str:
        mix = ", ".join(
            f"{language} {share * 100:.1f}%" for language, share in self.language_mix()
        )
        return (
            f"sngram-train stack-v3@{self.corpus.revision[:12]} "
            f"{self.counter.bytes_processed} content bytes "
            f"{self.counter.files_processed} files {self.state.repos} repos "
            f"vendor included; mix {mix}"
        )

    def _log_summary(self, complete: bool) -> None:
        self.events.log(
            "summary",
            complete=complete,
            decoded=self.state.decoded,
            repos=self.state.repos,
            vendor_files=self.state.vendor_files,
            retries=self.retries,
            wall_s=round(time.monotonic() - self.meter.started_at, 3),
        )

    def language_mix(self, top: int = _TOP_LANGS) -> list[tuple[str, float]]:
        with self.lock:
            total = sum(self.state.langs.values()) or 1
            ranked = sorted(self.state.langs.items(), key=lambda item: -item[1])
        return [(language, amount / total) for language, amount in ranked[:top]]

    def progress(self) -> float:
        if self.config.limit:
            return min(self.state.decoded / self.config.limit, 1.0)
        return self.state.shard_bytes / max(self.wire_target, 1)

    def rate_now(self) -> float:
        return self.meter.rate_now(self.state.decoded)

    def rate_avg(self) -> float:
        return self.meter.rate_avg(self.state.decoded)

    def wire_rate_now(self) -> float:
        return self.wire_meter.rate_now(self.state.shard_bytes)

    def wire_rate_avg(self) -> float:
        return self.wire_meter.rate_avg(self.state.shard_bytes)

    def eta_seconds(self) -> float | None:
        if self.config.limit:
            rate = self.rate_avg()
            remaining = max(self.config.limit - self.state.decoded, 0)
        else:
            rate = self.wire_rate_avg()
            remaining = max(self.wire_target - self.state.shard_bytes, 0)
        return remaining / rate if rate > 0 else None

    def describe_progress(self) -> str:
        from .units import fmt_bytes

        return f"{fmt_bytes(self.state.decoded)} decoded, {self.state.repos:,} repos"
