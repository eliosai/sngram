"""Streaming corpus trainer."""

from __future__ import annotations

import time
from collections import deque
from collections.abc import Callable
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path

import sngram

from . import metrics
from .checkpoint import RunState, load, save, write_table
from .events import EventLog
from .sampling import CountSink, WeightedDoc
from .stream import CorpusMeta, CorpusRow, CorpusStream

BATCH_DOCS = 128

StreamFactory = Callable[[dict | None], CorpusStream]


@dataclass(frozen=True)
class TrainerConfig:
    mint_dir: Path
    workers: int
    checkpoint_interval: float
    limit: int | None = None
    resume: bool = True


@dataclass(frozen=True)
class Fetched:
    row: CorpusRow
    data: bytes | None
    fetched_bytes: int
    error: str | None


def read_object(content, row: CorpusRow) -> Fetched:
    """Fetch one corpus row and re-encode it as UTF-8."""

    try:
        raw = content.read(row.blob_id, row.length)
        data = _utf8(raw, row.encoding)
        if not data:
            raise ValueError("decoded content is empty")
        return Fetched(row, data, len(raw), None)
    except (FileNotFoundError, LookupError, UnicodeError, ValueError) as error:
        return Fetched(row, None, 0, str(error)[:300])


def _utf8(raw: bytes, encoding: str) -> bytes:
    text = raw.decode(encoding)
    if encoding.lower() in ("utf-8", "utf8", "ascii", "us-ascii"):
        return raw
    return text.encode("utf-8")


class Trainer:
    def __init__(
        self,
        stream_factory: StreamFactory,
        content,
        config: TrainerConfig,
        corpus: CorpusMeta,
        on_refresh: Callable[[Trainer], None] | None = None,
    ) -> None:
        self.content = content
        self.config = config
        self.corpus = corpus
        self.on_refresh = on_refresh
        self._checkpoint_path = config.mint_dir / ".checkpoint.sqlite3"
        self.counter, self.state = self._load_state()
        self.stream = stream_factory(self.state.stream_state)
        self.committed_bytes = self.counter.bytes_processed
        self.events = EventLog(config.mint_dir / "train-events.jsonl")
        self.effective_target = min(
            self.config.limit or corpus.effective_bytes, corpus.effective_bytes
        )
        self.meter = metrics.RateMeter(baseline=self.committed_bytes)
        self.last_checkpoint_at: float | None = None
        self._sink = CountSink(self.counter)
        self._batch: list[WeightedDoc] = []

    def _load_state(self) -> tuple[sngram.BigramCounter, RunState]:
        corpus = self.corpus
        if not self.config.resume:
            return sngram.BigramCounter(), RunState(corpus.revision, corpus.corpus_id)
        return load(self._checkpoint_path, corpus.revision, corpus.corpus_id)

    def run(self) -> None:
        """Stream the corpus through the counter and mint the final table."""

        self.events.log(
            "start",
            corpus_rows=self.corpus.rows,
            target=self.effective_target,
            workers=self.config.workers,
        )
        complete = False
        try:
            with (
                ThreadPoolExecutor(max_workers=self.config.workers) as pool,
                ThreadPoolExecutor(max_workers=2) as counters,
            ):
                self._sink.pool = counters
                self._fill(pool)
            self._mint_final()
            complete = True
        finally:
            self._log_summary(complete)
            self.events.close()

    def _fill(self, pool: ThreadPoolExecutor) -> None:
        pending: deque[Future] = deque()
        rows = iter(self.stream)
        clean = True
        try:
            while self.committed_bytes < self.effective_target:
                row = next(rows, None)
                if row is None:
                    break
                while len(pending) >= self.config.workers * 4:
                    self._commit(pending.popleft().result())
                pending.append(pool.submit(read_object, self.content, row))
                self._maybe_checkpoint(pending)
        except BaseException:
            clean = False
            raise
        finally:
            self._settle(pending, clean)

    def _settle(self, pending: deque[Future], clean: bool) -> None:
        """Drain in-flight rows; checkpoint only a fully consistent quiesce."""

        try:
            while pending:
                self._commit(pending.popleft().result())
        except BaseException:
            if clean:
                raise
            return
        if clean:
            self._checkpoint()

    def _commit(self, fetched: Fetched) -> None:
        self.state.rows += 1
        if fetched.data is None:
            self._log_skip(fetched)
            return
        effective = len(fetched.data) * fetched.row.weight
        group = fetched.row.group
        self.state.fetched += fetched.fetched_bytes
        self.state.groups[group] = self.state.groups.get(group, 0) + effective
        self.committed_bytes += effective
        self._batch.append(WeightedDoc(fetched.data, fetched.row.weight))
        if len(self._batch) >= BATCH_DOCS:
            self._flush_batch()

    def _flush_batch(self) -> None:
        if self._batch:
            self._sink.submit(tuple(self._batch))
            self._batch = []
        self.meter.sample(self.committed_bytes)
        if self.on_refresh:
            self.on_refresh(self)

    def _log_skip(self, fetched: Fetched) -> None:
        self.state.skips += 1
        self.events.log(
            "content_skips",
            blob=fetched.row.blob_id,
            error=fetched.error or "content read failed",
        )

    def _maybe_checkpoint(self, pending: deque[Future]) -> None:
        last = self.last_checkpoint_at or self.meter.started_at
        if time.monotonic() - last < self.config.checkpoint_interval:
            return
        while pending:
            self._commit(pending.popleft().result())
        self._checkpoint()

    def _checkpoint(self) -> None:
        self._flush_batch()
        self._sink.drain()
        self.state.stream_state = self.stream.state_dict()
        save(self._checkpoint_path, self.counter, self.state)
        self.last_checkpoint_at = time.monotonic()
        self.events.log(
            "progress",
            effective_bytes=self.committed_bytes,
            fetched_bytes=self.state.fetched,
            rows=self.state.rows,
            rate=round(self.rate_now(), 1),
        )

    def _mint_final(self) -> None:
        self._flush_batch()
        self._sink.drain()
        if self.counter.bytes_processed != self.committed_bytes:
            raise RuntimeError("counter does not match committed progress")
        write_table(self.config.mint_dir, "final", self.counter, self._provenance())
        self.events.log(
            "mint",
            label="final",
            effective_bytes=self.counter.bytes_processed,
            fetched_bytes=self.state.fetched,
            rows=self.state.rows,
            groups=dict(sorted(self.state.groups.items())),
        )

    def _provenance(self) -> str:
        return (
            f"sngram-train stack-v2@{self.corpus.revision[:12]} "
            f"{self.counter.bytes_processed} effective bytes "
            f"{self.counter.files_processed} objects"
        )

    def _log_summary(self, complete: bool) -> None:
        self.events.log(
            "summary",
            complete=complete,
            effective_bytes=self.counter.bytes_processed,
            fetched_bytes=self.state.fetched,
            rows=self.state.rows,
            skips=self.state.skips,
            wall_s=round(time.monotonic() - self.meter.started_at, 3),
        )

    @property
    def skips(self) -> int:
        return self.state.skips

    def group_bytes(self) -> dict[str, int]:
        return dict(sorted(self.state.groups.items()))

    def rate_now(self) -> float:
        return self.meter.rate_now(self.committed_bytes)

    def describe_progress(self) -> str:
        from .units import fmt_bytes

        return f"{fmt_bytes(self.committed_bytes)} effective, {self.state.rows:,} rows"
