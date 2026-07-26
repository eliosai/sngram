"""Parallel corpus trainer."""

from __future__ import annotations

import gc
import shutil
import threading
import time
from collections import deque
from dataclasses import dataclass
from pathlib import Path

import sngram

from . import planner
from .checkpoint import RunState, load, save, write_table
from .corpus import Corpus, Shard, Sifted
from .errors import is_transient
from .events import EventLog
from .gate import Gate
from .progress import Progress
from .reader import open_reader

_RETRY_LIMIT = 6
_RETRY_BASE = 2.0
_JOIN_TIMEOUT = 120.0
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


@dataclass(frozen=True)
class GroupEnd:
    """How one row group ended: streamed out, capped, or stopped"""

    streamed: bool
    capped: bool = False


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
        self.progress = Progress(
            self.state, self.lock, config.limit, self.wire_target
        )
        self.last_checkpoint_at: float | None = None
        self._resumable, self._partial = self._positions()
        self._schedule = planner.schedule(
            corpus, self.state.stream_state, self.state.tallies
        )

    def _load_state(self) -> tuple[sngram.BigramCounter, RunState]:
        binding = self.corpus.identity.binding()
        if not self.config.resume:
            return sngram.BigramCounter(), RunState(binding)
        return load(self._checkpoint_path, binding)

    def _positions(self) -> tuple[deque[int], dict[int, tuple[int, int]]]:
        saved = self.state.stream_state or {}
        partial = {
            int(shard): (int(group), int(rows))
            for shard, group, rows in saved.get("partial", [])
        }
        return deque(sorted(partial)), partial

    def run(self) -> None:
        """Stream the corpus through the counter and mint the final table."""

        shutil.rmtree(self._spool_dir, ignore_errors=True)
        self._spool_dir.mkdir(parents=True, exist_ok=True)
        self.events.log(
            "start",
            corpus=self.corpus.identity.stamp(),
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
            self.progress.sample()
            if time.monotonic() - last_sweep >= _GC_PERIOD:
                gc.collect()
                last_sweep = time.monotonic()
            if self.config.limit and self.state.decoded >= self.config.limit:
                self.stop.set()
            elif self._checkpoint_due():
                self._checkpoint()

    def _checkpoint_due(self) -> bool:
        last = self.last_checkpoint_at or self.progress.decoded.started_at
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
            rows=self.state.rows,
            rate=round(self.progress.rate_now(), 1),
            mix={b: round(share, 4) for b, share in self.progress.mix()},
        )

    def _stream_state(self) -> dict:
        with self.lock:
            partial = [
                [shard, group, rows]
                for shard, (group, rows) in sorted(self._partial.items())
            ]
            return {**self._schedule.state(), "partial": partial}

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
            shard = self._schedule.next(self.state.tallies)
            if shard is None:
                return None
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
        self.events.log("retry", shard=shard, attempt=attempt, error=str(error)[:200])
        deadline = time.monotonic() + min(_RETRY_BASE * 2 ** (attempt - 1), 60.0)
        while time.monotonic() < deadline and not self.stop.is_set():
            self.gate.pause_point()
            time.sleep(0.2)

    def _read_shard(self, index: int, start_group: int, start_rows: int) -> None:
        shard = self.corpus.shards[index]
        path = self._spool(shard, index)
        if path is None:
            return
        try:
            with open(path, "rb") as handle:
                reader = open_reader(handle, shard)
                for group in range(start_group, reader.row_groups):
                    skip = start_rows if group == start_group else 0
                    end = self._read_group(reader, index, group, skip)
                    if end.capped:
                        break
                    if not end.streamed:
                        return
        finally:
            path.unlink(missing_ok=True)
        self._finish(index, shard)

    def _finish(self, index: int, shard: Shard) -> None:
        with self.lock:
            del self._partial[index]
            self.state.tallies.finish(shard)

    def _spool(self, shard: Shard, index: int) -> Path | None:
        path = self._spool_dir / f"shard-{index:05d}"
        with self.source.open(shard) as remote:
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

    def _read_group(self, reader, index: int, group: int, skip: int) -> GroupEnd:
        if not self._acquire_slot():
            return GroupEnd(streamed=False)
        batches = reader.batches(group, skip)
        try:
            rows = skip
            for sifted in batches:
                rows += sifted.rows
                if self._commit(sifted, index, group, rows):
                    return GroupEnd(streamed=True, capped=True)
                del sifted
                self.gate.pause_point()
                if self.stop.is_set():
                    return GroupEnd(streamed=False)
        finally:
            batches.close()
            self._slots.release()
        self._advance_shard(index, group, reader.row_groups)
        return GroupEnd(streamed=True)

    def _commit(self, sifted: Sifted, index: int, group: int, rows: int) -> bool:
        """Count one batch into the run; True when its bucket just filled up."""

        shard = self.corpus.shards[index]
        allowed = self._allowance(shard)
        if allowed is not None:
            sifted = sifted.head(allowed)
        staged = sngram.BigramCounter()
        staged.count_arrow(sifted.content)
        staged.add_files(sifted.files)
        self.counter.merge(staged)
        with self.lock:
            self._record(staged, sifted, shard, index, group, rows)
        return allowed is not None and staged.bytes_processed >= allowed

    def _allowance(self, shard: Shard) -> int | None:
        if self.corpus.quota is None:
            return None
        with self.lock:
            return self.corpus.quota.remaining(shard, self.state.tallies)

    def _record(
        self,
        staged: sngram.BigramCounter,
        sifted: Sifted,
        shard: Shard,
        index: int,
        group: int,
        rows: int,
    ) -> None:
        self.state.decoded += staged.bytes_processed
        self.state.rows += sifted.rows
        self.state.vendor_files += sifted.vendor_files
        self.state.tallies.add(shard, staged.bytes_processed)
        for language, amount in sifted.lang_bytes.items():
            self.state.langs[language] = self.state.langs.get(language, 0) + amount
        self._partial[index] = (group, rows)

    def _advance_shard(self, index: int, group: int, groups: int) -> None:
        size = self.corpus.shards[index].size
        share = size // groups
        if group == groups - 1:
            share = size - share * (groups - 1)
        with self.lock:
            self._partial[index] = (group + 1, 0)
            self.state.shard_bytes += share

    def _mint_final(self) -> None:
        if self.counter.bytes_processed != self.state.decoded:
            raise RuntimeError("counter does not match committed progress")
        write_table(self.config.mint_dir, "final", self.counter, self._provenance())
        self.events.log(
            "mint",
            label="final",
            corpus=self.corpus.identity.stamp(),
            decoded=self.state.decoded,
            rows=self.state.rows,
            files=self.counter.files_processed,
            vendor_files=self.state.vendor_files,
            languages=dict(sorted(self.state.langs.items())),
            families=dict(sorted(self.state.tallies.family_bytes.items())),
        )

    def _provenance(self) -> str:
        shares = self.progress.mix()
        mix = ", ".join(f"{bucket} {share * 100:.1f}%" for bucket, share in shares)
        return (
            f"sngram-train {self.corpus.identity.stamp()} "
            f"{self.counter.bytes_processed} content bytes "
            f"{self.counter.files_processed} files {self.state.rows} rows "
            f"vendor included; mix {mix}"
        )

    def _log_summary(self, complete: bool) -> None:
        self.events.log(
            "summary",
            complete=complete,
            decoded=self.state.decoded,
            rows=self.state.rows,
            vendor_files=self.state.vendor_files,
            retries=self.retries,
            wall_s=round(time.monotonic() - self.progress.decoded.started_at, 3),
        )
