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
import glob
import os
import queue
import shutil
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path

import sngram

from . import checkpoint, metrics
from .config import Family, Source, hf_token
from .events import EventLog
from .units import fmt_bytes, mint_label

MAX_HARD_ATTEMPTS = 3
QUEUE_DEPTH_PER_WORKER = 4
BATCH_ROWS = 256
# We read each shard's parquet file DIRECTLY (not through datasets' streaming
# iterator), because datasets retains every decompressed byte it reads — RSS
# grew ~1:1 with each multi-GB shard (measured +4.6 GB per 3.9 GB; ×16 workers
# => OOM). A direct read with a non-retaining fsspec cache (`cache_type=none`)
# and `pre_buffer=False` holds only the current row group, so per-worker memory
# is bounded by the row-group size and stays flat over a 50 TB run.
SHARD_CACHE_TYPE = "none"
SHARD_BLOCK_SIZE = 8 * 1024 * 1024
STALL_AFTER_S = 180.0
RETRY_BASE_S = 2.0
RETRY_CAP_S = 60.0
# early bootstrap mints (those below mint_every) before the steady cadence; with
# the default mint_every=1TB the schedule is 100gb, 500gb, then every 1TB
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


def normalized_weights(families: list[Family]) -> dict[str, float]:
    """Each family's target share of counted bytes, normalized to sum to 1."""
    total = sum(f.weight for f in families) or 1.0
    return {f.id: f.weight / total for f in families}


def estimated_family_bytes(
    counted: dict[str, int],
    completed: dict[str, int],
    dispatched: dict[str, int],
    failed: dict[str, int] | None = None,
) -> dict[str, float]:
    """Counted bytes per family plus an estimate of in-flight bytes (shards
    dispatched but neither counted nor failed), so the planner balances on what
    it has *committed and still expects to land*, not just what has *finished*.

    Without the in-flight term, the bounded queue is dead time: the planner would
    dispatch a whole queue of one family before any of it counted, then
    over-correct — the blend drifts over short windows (e.g. the first mint).
    Estimating in-flight with each family's observed mean shard size (a global
    mean until a family has a completion) closes that loop.

    `failed` (terminally failed shards — 404/gated/hard) must be subtracted:
    a dispatched shard that will never complete contributes no bytes, so leaving
    it in the in-flight count would make a dead source look like it is holding
    its share forever and silently skew the blend on a long, lossy run.
    """
    failed = failed or {}
    total_counted = sum(counted.values())
    total_done = sum(completed.values())
    global_mean = (total_counted / total_done) if total_done > 0 else 1.0
    est: dict[str, float] = {}
    for fid in dispatched:
        done = completed.get(fid, 0)
        mean = (counted.get(fid, 0) / done) if done > 0 else global_mean
        in_flight = max(dispatched.get(fid, 0) - done - failed.get(fid, 0), 0)
        est[fid] = counted.get(fid, 0) + mean * in_flight
    return est


def resume_dispatched(family_done: dict[str, int], order: list[str]) -> dict[str, int]:
    """Seed the planner's per-family dispatched counter from the restored
    completed-shard counts.

    `estimated_family_bytes` measures in-flight as `dispatched - completed`. On a
    fresh run both start at 0. On RESUME, `completed` is restored (e.g. 500) but
    `dispatched` is process-local — if it started at 0, `dispatched - completed`
    would clamp to 0 for the first `completed` dispatches, silently killing the
    in-flight correction for the whole post-resume run. Seeding `dispatched` to
    `completed` makes the difference measure post-resume in-flight from the start.
    """
    return {fid: family_done.get(fid, 0) for fid in order}


def pick_family(
    live: list[str], weights: dict[str, float], byte_estimate: dict[str, float]
) -> str:
    """The live family furthest below its target share of the (estimated) blend.

    Deficit = target_share - actual_share. Picking the max-deficit family each
    step drives the realized blend toward the weights while every family still
    has data. With nothing committed yet, seed from the highest-weight family.
    """
    total = sum(byte_estimate.values())
    if total <= 0:
        return max(live, key=lambda fid: weights.get(fid, 0.0))
    return max(
        live,
        key=lambda fid: weights.get(fid, 0.0) - byte_estimate.get(fid, 0.0) / total,
    )


def rss_bytes() -> int:
    try:
        with open("/proc/self/statm") as fh:
            return int(fh.read().split()[1]) * 4096
    except OSError:
        return 0


def _release_arrow_pool() -> None:
    """Return pyarrow's freed-but-retained buffers to the OS (best effort)."""
    try:
        import pyarrow as pa

        pa.default_memory_pool().release_unused()
    except Exception:  # noqa: BLE001 - purely advisory; never break a shard
        pass


def _attach_read_heartbeat(fh, ws: WorkerState) -> None:
    """Bump `ws.last_progress` on every read of `fh`.

    A worker decodes one parquet row group at a time; fetching a large row
    group's column chunk over the network can take longer than STALL_AFTER_S
    *before the first batch decodes*, during which `last_progress` (which the
    per-batch loop updates) would otherwise go stale and the watchdog would
    flag a hung connection — and the dashboard would redden a worker that is in
    fact downloading at full tilt. Bumping the clock on each underlying read
    keeps an actively-streaming worker alive; a genuinely dead connection still
    trips the watchdog because no reads land for STALL_AFTER_S.

    Best effort: if the handle forbids reassigning `read` (e.g. a C-level file),
    the worker simply falls back to per-batch heartbeats.
    """
    try:
        orig_read = fh.read

        def _read(*args, **kwargs):
            data = orig_read(*args, **kwargs)
            ws.last_progress = time.monotonic()
            return data

        fh.read = _read
    except (AttributeError, TypeError):  # immutable handle: degrade gracefully
        pass


def _resolved_files(ds) -> list[str]:
    """The ordered parquet file URLs behind a streaming `datasets` dataset.

    `datasets` resolves config/split/revision/glob into a concrete file list
    and hangs it on the Arrow examples iterable; we read those files directly
    (with bounded memory) instead of using its retaining streaming iterator.
    """
    ex = getattr(ds, "_ex_iterable", None)
    files = getattr(ex, "kwargs", {}).get("files") if ex is not None else None
    if not files:
        raise RuntimeError(
            "could not resolve parquet file list from the dataset "
            f"({type(ex).__name__ if ex is not None else 'no _ex_iterable'}); "
            "the datasets internal layout may have changed"
        )
    return list(files)


@dataclass
class ShardTask:
    source: Source
    shard: int
    n_shards: int
    revision: str | None
    attempts: int = 0
    retries: int = 0
    # True once the shard has been terminally accounted (completed or failed),
    # so the worker's catch-all can't double-count a shard whose only error was
    # a trailing advisory log write after it already committed
    accounted: bool = False


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
        # one resolved parquet-file-URL list per source, shared by planner +
        # workers: re-resolving metadata per shard would hammer the hub
        self._ds_cache: dict[str, list[str]] = {}
        self._ds_lock = threading.Lock()
        self._fs = None  # lazily-built HfFileSystem, shared by all workers
        self.worker_state = [WorkerState() for _ in range(workers)]
        self.stop = threading.Event()
        self.planner_done = threading.Event()
        self.in_flight_bytes = 0
        self.failed_shards = 0
        # terminally-failed shards per family (session-local, like the planner's
        # dispatched counter): subtracted from in-flight so a dead source can't
        # look like it is holding its blend share
        self._family_failed: dict[str, int] = {}
        self.errors = 0
        # per-family blend feedback (bytes + completed shards) lives on
        # self.state so it survives resume; it is mutated only under the merge
        # lock, alongside the counter merge and mark_done, so a checkpoint sees
        # a consistent cut
        self._lock = threading.Lock()
        # serializes counter merges + mark_done against checkpoint snapshots
        # and mints, so every snapshot/mint is a consistent cut: the counter
        # holds exactly the shards the state records as done
        self._merge_lock = threading.Lock()
        self.started_at = time.monotonic()
        self._rate_window: list[tuple[float, int]] = []
        # checkpoint status, surfaced in the dashboard header (not the event tail)
        self.last_checkpoint_at: float | None = None
        self.checkpoints_written = 0
        self.disk_free = 0
        # convergence signal: KL from the previous mint (whose count vector lives
        # on self.state, so it survives resume). Once KL stops shrinking, stop.
        self.last_kl: float | None = None

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
        """Weighted blend: feed the queue from whichever family is furthest below
        its target share of counted bytes, so every mint reflects the intended
        mix instead of the raw dataset sizes. A family whose shards are exhausted
        drops out and the rest keep the blend (finite code sets taper after they
        run dry; the steady mints hold the target ratio). One task per family is
        prefetched so a picked family never blocks the loop on its generator.

        Cold start caveat: until each family has a completed shard, in-flight is
        estimated with a global mean shard size, so the very first (100GB) mint's
        blend can be skewed by differing per-family shard sizes; it self-corrects
        as per-family means populate, well before the 1TB mint.
        """
        try:
            weights = normalized_weights(self.families)
            gens = {f.id: self._family_tasks(f) for f in self.families}
            order = [f.id for f in self.families]
            ahead = {fid: next(gens[fid], None) for fid in order}
            # seed from restored completed counts so the in-flight estimate is
            # live immediately after a resume (not clamped to 0 — see the fn)
            dispatched = resume_dispatched(self.state.family_done, order)
            for fid in order:
                if ahead[fid] is None:
                    self.events.log("family_done", family=fid)
            while not self.stop.is_set():
                live = [fid for fid in order if ahead.get(fid) is not None]
                if not live:
                    break
                with self._merge_lock:
                    counted = dict(self.state.family_bytes)
                    completed = dict(self.state.family_done)
                with self._lock:
                    failed = dict(self._family_failed)
                est = estimated_family_bytes(counted, completed, dispatched, failed)
                fid = pick_family(live, weights, est)
                task = ahead[fid]
                ahead[fid] = next(gens[fid], None)  # refill the prefetch slot
                if ahead[fid] is None:
                    self.events.log("family_done", family=fid)
                dispatched[fid] += 1  # count it now so the next pick sees in-flight
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

    def _load_source(self, source: Source) -> list[str]:
        """Resolve a source to its ordered list of parquet file URLs, pinned at
        its revision, and cache it.

        We let `datasets` do the hard resolution (config → split → revision →
        file glob) but then read the files ourselves (see `_run_shard`), so we
        extract the resolved, revision-stamped URL list rather than keeping the
        streaming dataset. The list order is the shard order — stable across
        restarts because the revision is pinned — so shard index == file index.
        """
        with self._ds_lock:
            cached = self._ds_cache.get(source.id)
        if cached is not None:
            return cached

        if source.data_files and "://" not in source.data_files:
            urls = sorted(glob.glob(source.data_files))
            if not urls:
                raise FileNotFoundError(f"local parquet glob not found: {source.data_files}")
            with self._ds_lock:
                self._ds_cache[source.id] = urls
            return urls

        from datasets import load_dataset

        rev = self._source_revision(source)
        if source.data_files:
            files = source.data_files
            if rev and "hf://datasets/" in files:
                head, rest = files.split("hf://datasets/", 1)
                org, repo_name, tail = rest.split("/", 2)
                files = f"{head}hf://datasets/{org}/{repo_name}@{rev}/{tail}"
            ds = load_dataset(
                "parquet", data_files=files, split="train",
                streaming=True, token=self.token,
            )
        else:
            ds = load_dataset(
                source.repo, name=source.config, split="train",
                streaming=True, token=self.token, revision=rev,
            )
        urls = _resolved_files(ds)
        with self._ds_lock:
            self._ds_cache[source.id] = urls
        return urls

    def _source_shards(self, source: Source) -> int:
        n = len(self._load_source(source))
        self.events.log("source", source=source.id, shards=n)
        return int(n)

    def _open_parquet(self, url: str, ws: WorkerState | None = None):
        """Open one shard's parquet for bounded-memory, row-group-at-a-time streaming.

        Returns (ParquetFile, file_handle|None). Remote (hf://) files open
        through a shared HfFileSystem with a non-retaining cache; local paths
        (test fixtures) open directly. `pre_buffer=False` keeps pyarrow from
        eagerly coalescing the whole file into memory. When `ws` is given every
        underlying network read bumps that worker's progress clock, so a long
        row-group fetch is not mistaken for a stall (see `_attach_read_heartbeat`).
        """
        import pyarrow.parquet as pq

        if "://" in url:
            if self._fs is None:
                with self._ds_lock:
                    if self._fs is None:
                        from huggingface_hub import HfFileSystem

                        self._fs = HfFileSystem(token=self.token)
            fh = self._fs.open(
                url, mode="rb",
                cache_type=SHARD_CACHE_TYPE, block_size=SHARD_BLOCK_SIZE,
            )
            if ws is not None:
                _attach_read_heartbeat(fh, ws)
            return pq.ParquetFile(fh, pre_buffer=False), fh
        return pq.ParquetFile(url, pre_buffer=False), None

    def _bump(self, name: str) -> None:
        with self._lock:
            setattr(self, name, getattr(self, name) + 1)

    def _mark_family_failed(self, family: str) -> None:
        # a terminally-failed shard never lands its bytes; record it so the
        # planner's in-flight estimate stops counting it (see estimated_family_bytes)
        with self._lock:
            self._family_failed[family] = self._family_failed.get(family, 0) + 1

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
                if not task.accounted:  # don't double-count an already-committed shard
                    self._mark_family_failed(task.source.family)
                self.events.log("error", stage="worker", worker=wid, error=err_text(e))
            finally:
                self.queue.task_done()
                ws.task = "idle"
                ws.stalled = False

    def _run_shard(self, ws: WorkerState, task: ShardTask) -> None:
        """Stream one shard (one parquet file) to completion a row group at a
        time, with rate-limit-aware retries.

        Each row group is counted into a throwaway sub-tally and folded into the
        file's tally only once it has streamed cleanly; the file commits to the
        shared counter exactly once, after every row group has landed. So a
        transient failure (429s, timeouts, connection resets, 5xx) re-reads only
        the in-progress row group — not the whole multi-GB file — and retries
        forever with exponential backoff: a rate-limit storm slows the run, it
        never loses shards or re-counts committed row groups. Missing data skips;
        anything else gets MAX_HARD_ATTEMPTS.

        `next_rg` advances past every committed row group so a retry resumes
        where it failed; `uncommitted` is the in-flight byte tally of the row
        group currently in progress, dropped on failure while the already-folded
        row groups stay in flight to merge when the retry finishes the file.
        """
        sid = f"{task.source.id}#{task.shard}"
        delay = RETRY_BASE_S
        file_tally = sngram.LocalTally()
        next_rg = 0
        uncommitted = 0
        ws.task = sid
        ws.shard_bytes = 0
        ws.started = time.monotonic()
        ws.last_progress = ws.started
        while not self.stop.is_set():
            try:
                url = self._load_source(task.source)[task.shard]
                pf, fh = self._open_parquet(url, ws)
                try:
                    # pyarrow silently yields empty batches for a missing column
                    # rather than raising — so a misnamed text field would count
                    # zero bytes in silence. Fail loudly instead.
                    field = task.source.text_field
                    names = pf.schema_arrow.names
                    if field not in names:
                        raise ValueError(
                            f"column [{field!r}] not in the dataset. "
                            f"columns in the dataset: {names}."
                        )
                    # pre_buffer=False + non-retaining cache + one row group per
                    # iter_batches call => only the current row group is resident,
                    # so memory stays bounded no matter how large the shard.
                    for rg in range(next_rg, pf.num_row_groups):
                        if self.stop.is_set():
                            self._drop_in_flight(ws)  # abandoned: nothing merged
                            return
                        rg_tally = sngram.LocalTally()
                        uncommitted = 0
                        for batch in pf.iter_batches(
                            batch_size=BATCH_ROWS, columns=[field], row_groups=[rg]
                        ):
                            if self.stop.is_set():
                                self._drop_in_flight(ws)
                                return
                            n = rg_tally.count_arrow(batch)
                            uncommitted += n
                            ws.shard_bytes += n
                            ws.last_progress = time.monotonic()
                            with self._lock:
                                self.in_flight_bytes += n
                        # the row group streamed cleanly: fold it in and advance
                        # the cursor so a later failure never re-reads it
                        file_tally.add_from(rg_tally)
                        uncommitted = 0
                        next_rg = rg + 1
                finally:
                    if fh is not None:
                        fh.close()
                    # hand this file's row-group buffers back to the OS so RSS
                    # resets between shards instead of creeping over a long run
                    _release_arrow_pool()
            except Exception as e:  # noqa: BLE001 - classified below, never fatal
                # discard only the in-progress row group; committed row groups
                # stay in flight and merge when the retry finishes the file
                if uncommitted:
                    with self._lock:
                        self.in_flight_bytes -= uncommitted
                    ws.shard_bytes -= uncommitted
                    uncommitted = 0
                self._bump("errors")
                kind = classify_error(e)
                if kind == "missing":
                    self._abandon_failed(ws, task, sid, kind, e)
                    return
                if kind == "hard":
                    task.attempts += 1
                    if task.attempts >= MAX_HARD_ATTEMPTS:
                        self._abandon_failed(ws, task, sid, kind, e)
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

            shard_bytes = ws.shard_bytes  # _drop_in_flight zeroes it
            # exactly-once: merge only after every row group streamed cleanly,
            # under the merge lock so checkpoints/mints see a consistent cut —
            # counter, mark_done, and the per-family blend feedback move together
            with self._merge_lock:
                self.counter.merge(file_tally)
                self.counter.add_files(1)
                self.state.mark_done(task.source.id, task.n_shards, task.shard, task.revision)
                fam = task.source.family
                self.state.family_bytes[fam] = self.state.family_bytes.get(fam, 0) + shard_bytes
                self.state.family_done[fam] = self.state.family_done.get(fam, 0) + 1
            self._drop_in_flight(ws)
            task.accounted = True  # committed; a failing log below must not undo this
            self.events.log(
                "shard",
                shard=sid,
                bytes=shard_bytes,
                secs=round(time.monotonic() - ws.started, 1),
            )
            return

    def _abandon_failed(
        self, ws: WorkerState, task: ShardTask, sid: str, kind: str, e: Exception
    ) -> None:
        """Terminally drop a shard (missing data, or hard error past its retries):
        committed-but-unmerged row groups never land, the family is debited so a
        dead source stops holding its blend share, and the task is accounted so
        the worker's catch-all cannot count it a second time."""
        self._drop_in_flight(ws)
        self._bump("failed_shards")
        self._mark_family_failed(task.source.family)
        task.accounted = True
        self.events.log(
            "error", stage="shard", shard=sid, error_kind=kind, error=err_text(e)
        )

    def _drop_in_flight(self, ws: WorkerState) -> None:
        with self._lock:
            self.in_flight_bytes -= ws.shard_bytes
        ws.shard_bytes = 0

    # ---------------------------------------------------------- supervisor

    async def run(self) -> None:
        pool = ThreadPoolExecutor(max_workers=self.workers + 1, thread_name_prefix="sngram")
        futures = [pool.submit(self._plan)]
        futures += [pool.submit(self._worker, i) for i in range(self.workers)]

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
            pool.shutdown(wait=True, cancel_futures=True)
            for future in futures:
                if future.done() and (exc := future.exception()) is not None:
                    self.events.log("error", stage="thread", error=err_text(exc))
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
            # under the merge lock the table and its count snapshot are a
            # consistent (total, counts) cut; a mint during a half-applied
            # merge would silently skew both
            table = self.counter.to_table_bytes()
            counts = metrics.counts_from_snapshot(self.counter.snapshot())
        tmp = path.with_suffix(".bin.tmp")
        tmp.write_bytes(table)
        os.replace(tmp, path)
        self.state.mints_done.append(label)
        # KL from the previous mint's distribution: the convergence signal. The
        # baseline lives on self.state, so it survives resume.
        kl = None
        if self.state.last_mint_counts is not None:
            kl = metrics.kl_divergence(counts, self.state.last_mint_counts)
            self.last_kl = kl
        self.state.last_mint_counts = counts
        self.events.log(
            "mint", label=label, path=str(path), bytes=self.durable_bytes(),
            pairs=self.counter.pairs_processed,
            kl_from_prev=round(kl, 6) if kl is not None else None,
        )

    def _checkpoint(self) -> None:
        with self._merge_lock:
            checkpoint.save(self._ckpt_dir, self.counter, self.state)
        free = shutil.disk_usage(self.mint_dir).free if self.mint_dir.exists() else 0
        # status lives in the header (live); the JSONL keeps the full beat as
        # debug material — it's split into small segments, not throttled
        self.last_checkpoint_at = time.monotonic()
        self.checkpoints_written += 1
        self.disk_free = free
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
        # a 60s window: long enough to smooth the lumps of multi-GB shards
        # merging, so the ETA doesn't jitter with every completion
        now = time.monotonic()
        self._rate_window.append((now, self.total_bytes()))
        while self._rate_window and now - self._rate_window[0][0] > 60.0:
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
        # measured against total_bytes (everything counted, incl. in-flight) —
        # the same quantity the headline shows. total_bytes advances every batch
        # so the ETA tracks the visible bar and trends down smoothly, instead of
        # freezing on durable (which only jumps when a whole shard merges).
        if not self.thresholds:
            return "—"
        remaining = self.thresholds[0] - self.total_bytes()
        if remaining <= 0:
            # bar is past the threshold; the mint fires once the in-flight
            # shards merge and durable catches up — moments away
            return "soon"
        rate = self.rate_now() or self.rate_avg()
        if rate <= 0:
            return "—"
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
