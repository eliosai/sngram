"""The training pipeline: stream, count, merge, mint.

Shape: a *planner* thread walks the dataset families round-robin and feeds a
bounded queue of shard tasks (so the mix stays blended); N *worker* threads
pull tasks, stream one shard each as Arrow batches, and count through the
GIL-free Rust tally; a completed shard's tally merges into the one shared
counter (exactly-once: a failed shard contributes nothing). An asyncio
*supervisor* owns everything slow-moving — dashboard, mint thresholds,
checkpoints, the stall watchdog, the byte limit.
"""

from __future__ import annotations

import asyncio
import os
import queue
import shutil
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path

import sngram

from . import checkpoint
from .config import Family, Source, hf_token
from .events import EventLog
from .units import fmt_bytes, mint_label

MAX_HARD_ATTEMPTS = 3
QUEUE_DEPTH_PER_WORKER = 4
BATCH_ROWS = 256
STALL_AFTER_S = 180.0
RETRY_BASE_S = 2.0
RETRY_CAP_S = 60.0
# early mints before the steady every-5TB cadence
BOOTSTRAP_MINTS = [100 * 10**9, 500 * 10**9, 10**12]

_TRANSIENT_MARKERS = (
    "429", "too many requests", "rate limit", "throttl", "timeout", "timed out",
    "connection", "reset by peer", "broken pipe", "temporarily", "500", "502",
    "503", "504", "incompleteread", "chunkedencoding", "ssl", "eof occurred",
    "disconnected", "payload", "slowdown", "serviceunavailable", "internalerror",
    "protocolerror", "remote end closed", "unavailable",
)
_NOT_FOUND_MARKERS = ("404", "not found", "does not exist", "gated")


def _chain_text(e: BaseException) -> str:
    """The exception plus its whole cause/context chain, lowered — wrappers
    like DatasetGenerationError must not hide a transient root cause."""
    parts = []
    seen: set[int] = set()
    cur: BaseException | None = e
    while cur is not None and id(cur) not in seen and len(parts) < 8:
        seen.add(id(cur))
        parts.append(f"{type(cur).__name__}: {cur}")
        cur = cur.__cause__ or cur.__context__
    return " | ".join(parts).lower()


def classify_error(e: Exception) -> str:
    """transient (retry forever with backoff) | missing (skip) | hard (bounded)."""
    s = _chain_text(e)
    if any(m in s for m in _NOT_FOUND_MARKERS):
        return "missing"
    if any(m in s for m in _TRANSIENT_MARKERS):
        return "transient"
    return "hard"


def err_text(e: Exception, limit: int = 400) -> str:
    """Bounded error description for event logs (a 500-page body is not a log line)."""
    return _chain_text(e)[:limit]


def default_workers() -> int:
    """One worker per physical core, clamped to 4..16: counting saturates a
    core's load units with one thread (HT adds ~0), and each worker is mostly
    network-blocked anyway."""
    logical = __import__("os").cpu_count() or 8
    return max(4, min(16, logical // 2))


def rss_bytes() -> int:
    try:
        with open("/proc/self/statm") as fh:
            return int(fh.read().split()[1]) * 4096
    except OSError:
        return 0


@dataclass
class ShardTask:
    source: Source
    shard: int
    n_shards: int
    revision: str | None
    attempts: int = 0
    retries: int = 0


@dataclass
class WorkerState:
    """What one worker is doing right now (dashboard + watchdog input)."""

    task: str = "idle"
    shard_bytes: int = 0
    started: float = 0.0
    last_progress: float = field(default_factory=time.monotonic)
    stalled: bool = False


class Trainer:
    def __init__(
        self,
        families: list[Family],
        mint_dir: Path,
        target: int,
        mint_every: int,
        workers: int,
        limit: int | None,
        checkpoint_every_s: float,
        resume: bool,
        on_refresh=None,
    ) -> None:
        self.families = families
        self.mint_dir = mint_dir
        self.target = target
        self.limit = limit
        self.workers = workers
        self.checkpoint_every_s = checkpoint_every_s
        self.on_refresh = on_refresh

        self.token = hf_token()
        self.counter = sngram.BigramCounter()
        self.events = EventLog(mint_dir / "train-events.jsonl")
        self.state = checkpoint.RunState()
        if resume:
            restored = checkpoint.load(self._ckpt_dir, self.counter)
            if restored is not None:
                self.state = restored
                self.events.log(
                    "resume",
                    bytes=self.counter.bytes_processed,
                    files=self.counter.files_processed,
                    mints=len(self.state.mints_done),
                )

        schedule = sorted(
            {t for t in BOOTSTRAP_MINTS if t < mint_every}
            | set(range(mint_every, target + 1, mint_every))
        )
        self.thresholds = [
            t for t in schedule if t <= target and mint_label(t) not in self.state.mints_done
        ]

        self.queue: queue.Queue[ShardTask] = queue.Queue(
            maxsize=QUEUE_DEPTH_PER_WORKER * workers
        )
        # one resolved IterableDataset per source, shared by planner + workers:
        # re-resolving metadata per shard would hammer the hub for nothing
        self._ds_cache: dict[str, object] = {}
        self._ds_lock = threading.Lock()
        self.worker_state = [WorkerState() for _ in range(workers)]
        self.stop = threading.Event()
        self.planner_done = threading.Event()
        self.in_flight_bytes = 0
        self.failed_shards = 0
        self.errors = 0
        self._lock = threading.Lock()
        # serializes counter merges + mark_done against checkpoint snapshots
        # and mints, so every snapshot/mint is a consistent cut: the counter
        # holds exactly the shards the state records as done
        self._merge_lock = threading.Lock()
        self.started_at = time.monotonic()
        self._rate_window: list[tuple[float, int]] = []

    # ------------------------------------------------------------- totals

    @property
    def _ckpt_dir(self) -> Path:
        return self.mint_dir / ".checkpoint"

    def total_bytes(self) -> int:
        """Durable (merged) bytes + live in-flight bytes, for display."""
        with self._lock:
            return self.counter.bytes_processed + self.in_flight_bytes

    def durable_bytes(self) -> int:
        return self.counter.bytes_processed

    # ------------------------------------------------------------- planner

    def _plan(self) -> None:
        """Round-robin the families, lazily expanding each into shard tasks."""
        try:
            gens = {f.id: self._family_tasks(f) for f in self.families}
            order = [f.id for f in self.families]
            while order and not self.stop.is_set():
                for fid in list(order):
                    gen = gens[fid]
                    task = next(gen, None)
                    if task is None:
                        order.remove(fid)
                        self.events.log("family_done", family=fid)
                        continue
                    while not self.stop.is_set():
                        try:
                            self.queue.put(task, timeout=1.0)
                            break
                        except queue.Full:
                            continue
        except Exception as e:  # noqa: BLE001 - planner death must be loud, not silent
            self._bump("errors")
            self.events.log("error", stage="planner", error=err_text(e))
        finally:
            self.planner_done.set()

    def _family_tasks(self, family: Family):
        for source in family.sources:
            if self.stop.is_set():
                return
            n = self._resolve_source(source)
            if n is None:
                continue
            rev = self._source_revision(source)
            for shard in range(n):
                if not self.state.is_done(source.id, n, shard, rev):
                    yield ShardTask(source, shard, n, rev)

    def _resolve_source(self, source: Source) -> int | None:
        """Shard count for a source, with retry discipline: a transient blip
        at plan time must not drop thousands of shards. Unlike shard retries,
        plan-time transient retries are CAPPED — the planner is one thread, so
        a single perpetually-throttled source must not starve every family.
        A skipped source's shards are unmarked, so the next resume retries it.
        """
        delay = RETRY_BASE_S
        hard, transient = 0, 0
        while not self.stop.is_set():
            try:
                return self._source_shards(source)
            except Exception as e:  # noqa: BLE001 - classified below
                self._bump("errors")
                kind = classify_error(e)
                if kind == "transient":
                    transient += 1
                elif kind == "hard":
                    hard += 1
                if kind == "missing" or hard >= MAX_HARD_ATTEMPTS or transient >= 8:
                    self.events.log(
                        "error", stage="plan", source=source.id,
                        error_kind=kind, error=err_text(e),
                    )
                    return None
                self.events.log(
                    "warn", stage="plan", source=source.id, error_kind=kind,
                    retry_in_s=round(delay), error=err_text(e),
                )
                self.stop.wait(delay)
                delay = min(delay * 2, RETRY_CAP_S)
        return None

    def _repo_revision(self, repo: str) -> str:
        """Pin one commit sha per repo for the whole run (and its restarts):
        a repo commit mid-run must never shift shard indices under us."""
        with self._merge_lock:
            if repo in self.state.revisions:
                return self.state.revisions[repo]
        from huggingface_hub import HfApi

        sha = HfApi(token=self.token).dataset_info(repo).sha
        with self._merge_lock:
            self.state.revisions.setdefault(repo, sha)
            return self.state.revisions[repo]

    def _source_revision(self, source: Source) -> str | None:
        if source.data_files and "hf://datasets/" not in source.data_files:
            return None  # local files (tests, fixtures): nothing to pin
        repo = source.repo if not source.data_files else source.data_files.split(
            "hf://datasets/"
        )[1].split("/")[0] + "/" + source.data_files.split("hf://datasets/")[1].split("/")[1]
        return self._repo_revision(repo)

    def _load_source(self, source: Source):
        """Resolve a source once, at its pinned revision, and cache it."""
        with self._ds_lock:
            cached = self._ds_cache.get(source.id)
        if cached is not None:
            return cached

        from datasets import load_dataset

        rev = self._source_revision(source)
        if source.data_files:
            files = source.data_files
            if rev and "hf://datasets/" in files:
                head, rest = files.split("hf://datasets/", 1)
                org, repo_name, tail = rest.split("/", 2)
                files = f"{head}hf://datasets/{org}/{repo_name}@{rev}/{tail}"
            ds = load_dataset(
                "parquet",
                data_files=files,
                split="train",
                streaming=True,
                token=self.token,
            )
        else:
            ds = load_dataset(
                source.repo,
                name=source.config,
                split="train",
                streaming=True,
                token=self.token,
                revision=rev,
            )
        with self._ds_lock:
            self._ds_cache[source.id] = ds
        return ds

    def _source_shards(self, source: Source) -> int:
        ds = self._load_source(source)
        n = getattr(ds, "num_shards", None) or getattr(ds, "n_shards")
        self.events.log("source", source=source.id, shards=n)
        return int(n)

    def _bump(self, name: str) -> None:
        with self._lock:
            setattr(self, name, getattr(self, name) + 1)

    # ------------------------------------------------------------- workers

    def _worker(self, wid: int) -> None:
        ws = self.worker_state[wid]
        while not self.stop.is_set():
            if self.limit is not None and self.counter.bytes_processed >= self.limit:
                return  # the limit is durable; don't start more work
            try:
                task = self.queue.get(timeout=1.0)
            except queue.Empty:
                if self.planner_done.is_set():
                    return
                continue
            # queue.unfinished_tasks now covers this shard until task_done(),
            # so the supervisor cannot see "no work" while we hold it
            try:
                self._run_shard(ws, task)
            except Exception as e:  # noqa: BLE001 - a worker must never die silently
                self._bump("errors")
                self.events.log("error", stage="worker", worker=wid, error=err_text(e))
            finally:
                self.queue.task_done()
                ws.task = "idle"
                ws.stalled = False

    def _run_shard(self, ws: WorkerState, task: ShardTask) -> None:
        """Stream one shard to completion, with rate-limit-aware retries.

        Transient failures (429s, timeouts, connection resets, 5xx) retry
        forever with exponential backoff — a rate-limit storm slows the run,
        it never loses shards. Missing data skips; anything else gets
        MAX_HARD_ATTEMPTS. Every attempt starts a fresh tally, so the counter
        only ever sees a shard exactly once.
        """
        sid = f"{task.source.id}#{task.shard}"
        delay = RETRY_BASE_S
        while not self.stop.is_set():
            ws.task = sid
            ws.shard_bytes = 0
            ws.started = time.monotonic()
            ws.last_progress = ws.started
            tally = sngram.LocalTally()
            try:
                ds = self._load_source(task.source)
                with self._ds_lock:
                    # transform-building on the shared dataset is functional
                    # but not documented thread-safe; it is cheap, so serialize
                    shard = (
                        ds.shard(num_shards=task.n_shards, index=task.shard)
                        .select_columns([task.source.text_field])
                        .with_format("arrow")
                    )
                for batch in shard.iter(batch_size=BATCH_ROWS):
                    if self.stop.is_set():
                        # abandoned: nothing merged, shard not marked done
                        self._drop_in_flight(ws)
                        return
                    n = tally.count_arrow(batch)
                    ws.shard_bytes += n
                    ws.last_progress = time.monotonic()
                    with self._lock:
                        self.in_flight_bytes += n
            except Exception as e:  # noqa: BLE001 - classified below, never fatal
                self._drop_in_flight(ws)
                self._bump("errors")
                kind = classify_error(e)
                if kind == "missing":
                    self._bump("failed_shards")
                    self.events.log(
                        "error", stage="shard", shard=sid, error_kind=kind, error=err_text(e)
                    )
                    return
                if kind == "hard":
                    task.attempts += 1
                    if task.attempts >= MAX_HARD_ATTEMPTS:
                        self._bump("failed_shards")
                        self.events.log(
                            "error", stage="shard", shard=sid, error_kind=kind, error=err_text(e)
                        )
                        return
                task.retries += 1
                # a long 429 storm must not write millions of identical lines
                if task.retries <= 3 or task.retries % 10 == 0:
                    self.events.log(
                        "warn", stage="shard", shard=sid, error_kind=kind,
                        retries=task.retries, retry_in_s=round(delay), error=err_text(e),
                    )
                ws.task = f"{sid} (retry in {delay:.0f}s)"
                self.stop.wait(delay)  # interruptible backoff
                delay = min(delay * 2, RETRY_CAP_S)
                continue

            # exactly-once: merge only after the whole shard streamed cleanly,
            # under the merge lock so checkpoints/mints see a consistent cut
            with self._merge_lock:
                self.counter.merge(tally)
                self.counter.add_files(1)
                self.state.mark_done(task.source.id, task.n_shards, task.shard, task.revision)
            shard_bytes = ws.shard_bytes  # _drop_in_flight zeroes it
            self._drop_in_flight(ws)
            self.events.log(
                "shard",
                shard=sid,
                bytes=shard_bytes,
                secs=round(time.monotonic() - ws.started, 1),
            )
            return

    def _drop_in_flight(self, ws: WorkerState) -> None:
        with self._lock:
            self.in_flight_bytes -= ws.shard_bytes
        ws.shard_bytes = 0

    # ---------------------------------------------------------- supervisor

    async def run(self) -> None:
        loop = asyncio.get_running_loop()
        pool = ThreadPoolExecutor(max_workers=self.workers + 1, thread_name_prefix="sngram")
        futures = [loop.run_in_executor(pool, self._plan)]
        futures += [loop.run_in_executor(pool, self._worker, i) for i in range(self.workers)]

        last_ckpt = time.monotonic()
        finished = False
        try:
            while not self.stop.is_set():
                await asyncio.sleep(0.25)
                self._mint_if_due()
                self._watchdog()
                self._rate_sample()
                if self.on_refresh:
                    self.on_refresh(self)
                if time.monotonic() - last_ckpt >= self.checkpoint_every_s:
                    self._checkpoint()
                    last_ckpt = time.monotonic()
                if self.limit is not None and self.durable_bytes() >= self.limit:
                    self.events.log("limit", bytes=self.durable_bytes())
                    finished = True
                    break
                if self.durable_bytes() >= self.target:
                    self.events.log("target", bytes=self.durable_bytes())
                    finished = True
                    break
                # unfinished_tasks covers queued AND in-progress shards in one
                # atomic counter (task_done fires only when a worker finishes),
                # so this cannot race a worker between dequeue and execution
                if self.planner_done.is_set() and self.queue.unfinished_tasks == 0:
                    self.events.log("exhausted", bytes=self.durable_bytes())
                    finished = True
                    break
        finally:
            self.stop.set()
            await asyncio.gather(*futures, return_exceptions=True)
            pool.shutdown(wait=True)
            self._mint_if_due()
            if finished:
                # only a run that reached its end mints "final"; an interrupted
                # or crashed run checkpoints and resumes instead
                self._mint("final")
            try:
                # the shutdown checkpoint must survive disk trouble and Ctrl-C
                # delivered mid-finally: "checkpoint saved" must never be a lie
                self._checkpoint()
            except BaseException as e:  # noqa: BLE001
                self.events.log("error", stage="shutdown_checkpoint", error=err_text(e))
            try:
                self.events.log(
                    "summary",
                    bytes=self.durable_bytes(),
                    files=self.counter.files_processed,
                    pairs=self.counter.pairs_processed,
                    errors=self.errors,
                    failed_shards=self.failed_shards,
                    rss=rss_bytes(),
                    wall_s=round(time.monotonic() - self.started_at, 1),
                )
            finally:
                self.events.close()

    # ------------------------------------------------------------- minting

    def _mint_if_due(self) -> None:
        while self.thresholds and self.durable_bytes() >= self.thresholds[0]:
            threshold = self.thresholds.pop(0)
            self._mint(mint_label(threshold))
            self._checkpoint()

    def _mint(self, label: str) -> None:
        if label in self.state.mints_done:
            return
        path = self.mint_dir / f"{label}_weights.bin"
        self.mint_dir.mkdir(parents=True, exist_ok=True)
        with self._merge_lock:
            # under the merge lock the table is a consistent (total, counts)
            # pair; a mint during a half-applied merge would silently skew it
            table = self.counter.to_table_bytes()
        tmp = path.with_suffix(".bin.tmp")
        tmp.write_bytes(table)
        os.replace(tmp, path)
        self.state.mints_done.append(label)
        self.events.log(
            "mint", label=label, path=str(path), bytes=self.durable_bytes(),
            pairs=self.counter.pairs_processed,
        )

    def _checkpoint(self) -> None:
        with self._merge_lock:
            checkpoint.save(self._ckpt_dir, self.counter, self.state)
        free = shutil.disk_usage(self.mint_dir).free if self.mint_dir.exists() else 0
        self.events.log(
            "checkpoint", bytes=self.durable_bytes(), rss=rss_bytes(), disk_free=free
        )
        if 0 < free < 5 * 10**9:
            self.events.log("warn", stage="disk", free=free)

    # ----------------------------------------------------------- liveness

    def _watchdog(self) -> None:
        now = time.monotonic()
        for i, ws in enumerate(self.worker_state):
            if ws.task == "idle":
                continue
            age = now - ws.last_progress
            if age > STALL_AFTER_S and not ws.stalled:
                ws.stalled = True
                self.events.log("stall", worker=i, shard=ws.task, silent_s=round(age))
            elif age <= STALL_AFTER_S:
                ws.stalled = False

    def _rate_sample(self) -> None:
        now = time.monotonic()
        self._rate_window.append((now, self.total_bytes()))
        while self._rate_window and now - self._rate_window[0][0] > 30.0:
            self._rate_window.pop(0)

    def rate_now(self) -> float:
        if len(self._rate_window) < 2:
            return 0.0
        (t0, b0), (t1, b1) = self._rate_window[0], self._rate_window[-1]
        return (b1 - b0) / max(t1 - t0, 1e-6)

    def rate_avg(self) -> float:
        elapsed = max(time.monotonic() - self.started_at, 1e-6)
        return self.total_bytes() / elapsed

    def eta_next_mint(self) -> str:
        if not self.thresholds:
            return "—"
        remaining = self.thresholds[0] - self.durable_bytes()
        rate = self.rate_now() or self.rate_avg()
        if rate <= 0:
            return "∞"
        secs = remaining / rate
        if secs > 86_400:
            return f"{secs / 86_400:.1f} d"
        if secs > 3_600:
            return f"{secs / 3_600:.1f} h"
        return f"{secs / 60:.0f} min"

    def describe_progress(self) -> str:
        return (
            f"{fmt_bytes(self.total_bytes())} counted, "
            f"{self.counter.files_processed} shards, "
            f"{len(self.state.mints_done)} mints"
        )
