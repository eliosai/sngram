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
import gzip
import hashlib
import json
import os
import queue
import re
import shutil
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import urlparse

import sngram

from . import checkpoint, metrics
from .config import Family, Source, hf_token, stack_v2_bucket_for, stack_v2_skip_reason
from .events import EventLog
from .units import fmt_bytes, mint_label

MAX_HARD_ATTEMPTS = 3
QUEUE_DEPTH_PER_WORKER = 4
BATCH_ROWS = 256
# We read each shard's parquet/raw file DIRECTLY (not through datasets'
# streaming iterator), because datasets retains decompressed bytes and can grow
# RSS ~1:1 with large shards. Remote reads use bounded fsspec readahead instead
# of `cache_type=none`: no-cache caused storms of tiny HF/Xet range requests,
# synchronized worker stalls, and extreme rate limiting. With pre_buffer=False,
# memory is bounded by the active row group plus one readahead block per worker.
SHARD_CACHE_TYPE = "readahead"
SHARD_BLOCK_SIZE = 64 * 1024 * 1024
DEFAULT_REMOTE_STREAMS = 4
JSON_BATCH_ROWS = 64
JSON_BATCH_BYTES = 8 * 1024 * 1024
SWH_METADATA_BATCH_ROWS = 2048
SWH_TEXT_BATCH_ROWS = 64
SWH_TEXT_BATCH_BYTES = 8 * 1024 * 1024
SWH_OBJECT_ATTEMPTS = 3
SWH_SLOW_OBJECT_S = float(os.environ.get("SNG_SWH_SLOW_OBJECT_S", "15"))
READ_GAP_WARN_S = 60.0
STALL_AFTER_S = 180.0
RETRY_BASE_S = 2.0
RETRY_CAP_S = 60.0
MEMORY_SOFT_LIMIT_BYTES = 5 * 10**9
MEMORY_TRIM_INTERVAL_S = 5.0
# early bootstrap mints (those below mint_every) before the steady cadence; with
# the default mint_every=1TB the schedule is 100gb, 500gb, then every 1TB
BOOTSTRAP_MINTS = [100 * 10**9, 500 * 10**9, 10**12]

_TRANSIENT_MARKERS = (
    "too many requests", "rate limit", "throttl", "timeout", "timed out",
    "connection", "reset by peer", "broken pipe", "temporarily",
    "incompleteread", "chunkedencoding", "ssl", "eof occurred",
    "disconnected", "payload", "slowdown", "serviceunavailable", "internalerror",
    "protocolerror", "remote end closed", "unavailable",
)
_TRANSIENT_STATUS_RE = re.compile(r"\b(?:429|500|502|503|504)\b")
_NOT_FOUND_MARKERS = ("not found", "does not exist", "gated")
_NOT_FOUND_STATUS_RE = re.compile(r"\b404\b")
_INCOMPLETE_BODY_RE = re.compile(r"received\s+(\d+)\s+bytes,\s+expected\s+(\d+)")
_ERROR_TYPES = (
    "remoteprotocolerror",
    "chunkedencodingerror",
    "incompleteread",
    "serverdisconnectederror",
    "clientpayloaderror",
    "connectionreseterror",
    "timeout",
)


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
    if any(m in s for m in _TRANSIENT_MARKERS) or _TRANSIENT_STATUS_RE.search(s):
        return "transient"
    if any(m in s for m in _NOT_FOUND_MARKERS) or _NOT_FOUND_STATUS_RE.search(s):
        return "missing"
    return "hard"


def err_text(e: Exception, limit: int = 400) -> str:
    """Bounded error description for event logs (a 500-page body is not a log line)."""
    return _chain_text(e)[:limit]


def error_debug_fields(e: Exception) -> dict[str, object]:
    """Small structured fields that make retry logs grep-able."""
    s = _chain_text(e)
    fields: dict[str, object] = {}
    for error_type in _ERROR_TYPES:
        if error_type in s:
            fields["error_type"] = error_type
            break
    if m := _INCOMPLETE_BODY_RE.search(s):
        fields["received_bytes"] = int(m.group(1))
        fields["expected_bytes"] = int(m.group(2))
    return fields


def _latency_ms(values: list[float], q: float) -> int:
    if not values:
        return 0
    ordered = sorted(values)
    idx = min(len(ordered) - 1, max(0, int((len(ordered) - 1) * q)))
    return int(ordered[idx] * 1000)


def _stats_skip(stats: dict[str, int], reason: str) -> None:
    stats[f"skipped_{reason}"] = stats.get(f"skipped_{reason}", 0) + 1


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


def roster_hash(families: list[Family], target: int, mint_every: int) -> str:
    """Stable identity of the distribution contract used by checkpoints."""
    payload = {
        "target": target,
        "mint_every": mint_every,
        "families": [
            {
                "id": f.id,
                "weight": f.weight,
                "cap_bytes": f.cap_bytes,
                "sources": [
                    {
                        "id": s.id,
                        "family": s.family,
                        "repo": s.repo,
                        "config": s.config,
                        "text_field": s.text_field,
                        "cap_bytes": s.cap_bytes,
                        "format": s.format,
                        "data_files": s.data_files,
                        "content_prefix": s.content_prefix,
                        "metadata_fields": s.metadata_fields,
                    }
                    for s in f.sources
                ],
            }
            for f in families
        ],
    }
    encoded = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def rss_bytes() -> int:
    try:
        with open("/proc/self/statm") as fh:
            return int(fh.read().split()[1]) * 4096
    except OSError:
        return 0


def memory_stats() -> dict[str, object]:
    stats: dict[str, object] = {
        "rss": rss_bytes(),
        "arrow_bytes": 0,
        "arrow_max": 0,
        "arrow_backend": "unavailable",
    }
    try:
        import pyarrow as pa

        pool = pa.default_memory_pool()
        stats["arrow_bytes"] = pool.bytes_allocated()
        stats["arrow_max"] = pool.max_memory() or 0
        stats["arrow_backend"] = pool.backend_name
    except Exception:  # noqa: BLE001 - telemetry must never break training
        pass
    return stats


def _release_arrow_pool() -> None:
    """Return pyarrow's freed-but-retained buffers to the OS (best effort)."""
    try:
        import pyarrow as pa

        pa.default_memory_pool().release_unused()
    except Exception:  # noqa: BLE001 - purely advisory; never break a shard
        pass


def _collect_python_memory() -> None:
    try:
        import gc

        gc.collect()
    except Exception:  # noqa: BLE001 - purely advisory; never break a shard
        pass


def _trim_process_memory() -> bool:
    """Ask glibc malloc to return free arenas to the OS (best effort)."""
    if os.name != "posix":
        return False
    try:
        import ctypes

        libc = ctypes.CDLL("libc.so.6")
        return bool(libc.malloc_trim(0))
    except Exception:  # noqa: BLE001 - absent on non-glibc platforms
        return False


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


class _HeartbeatFile:
    """File wrapper for gzip/raw streams where monkey-patching read is unreliable."""

    def __init__(self, fh, ws: WorkerState) -> None:
        self._fh = fh
        self._ws = ws

    def _beat(self):
        self._ws.last_progress = time.monotonic()

    def read(self, *args, **kwargs):
        data = self._fh.read(*args, **kwargs)
        self._beat()
        return data

    def read1(self, *args, **kwargs):
        data = self._fh.read1(*args, **kwargs)
        self._beat()
        return data

    def readinto(self, *args, **kwargs):
        n = self._fh.readinto(*args, **kwargs)
        self._beat()
        return n

    def readline(self, *args, **kwargs):
        line = self._fh.readline(*args, **kwargs)
        self._beat()
        return line

    def close(self) -> None:
        fh = self._fh
        if fh is None:
            return
        try:
            response = getattr(fh, "response", None)
            if response is not None:
                response.close()
            exit_stack = getattr(fh, "_exit_stack", None)
            if exit_stack is not None:
                exit_stack.close()
            stream_buffer = getattr(fh, "_stream_buffer", None)
            if stream_buffer is not None:
                stream_buffer.clear()
        finally:
            try:
                fh.close()
            finally:
                self._fh = None

    def __enter__(self):
        self._fh.__enter__()
        return self

    def __exit__(self, *exc):
        self.close()
        return False

    def __iter__(self):
        return self

    def __next__(self):
        line = self.readline()
        if line:
            return line
        raise StopIteration

    def __getattr__(self, name: str):
        return getattr(self._fh, name)


def _utf8_prefix(value: str, limit: int) -> str:
    clipped = value.encode("utf-8")[:limit]
    while clipped:
        try:
            return clipped.decode("utf-8")
        except UnicodeDecodeError as e:
            clipped = clipped[:e.start]
    return ""


def _prefix_values_to_bytes(values: list[object], limit: int) -> list[str]:
    out: list[str] = []
    used = 0
    for value in values:
        if not isinstance(value, str):
            continue
        encoded_len = len(value.encode("utf-8"))
        if used + encoded_len <= limit:
            out.append(value)
            used += encoded_len
            continue
        room = limit - used
        if room > 0:
            prefix = _utf8_prefix(value, room)
            if prefix:
                out.append(prefix)
        break
    return out


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
    stall_started: float = 0.0
    stall_count: int = 0
    max_silent_s: float = 0.0
    last_gap_log: float = 0.0


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
        self._families = {f.id: f for f in families}
        self._source_caps = {s.id: s.cap_bytes for f in families for s in f.sources}
        self.mint_dir = mint_dir
        self.target = target
        self.limit = limit
        self.mint_every = mint_every
        self.workers = workers
        self.checkpoint_every_s = checkpoint_every_s
        self.on_refresh = on_refresh
        self.roster_hash = roster_hash(families, target, mint_every)

        self.token = hf_token()
        self.counter = sngram.BigramCounter()
        self.events = EventLog(mint_dir / "train-events.jsonl")
        self.state = checkpoint.RunState()
        self.state.roster_hash = self.roster_hash
        if resume:
            restored = checkpoint.load(self._ckpt_dir, self.counter)
            if restored is not None:
                if (
                    restored.roster_hash is not None
                    and restored.roster_hash != self.roster_hash
                ):
                    raise RuntimeError(
                        "distribution roster changed since checkpoint; "
                        "start a new mint dir or restore the original caps/sources"
                    )
                restored.roster_hash = restored.roster_hash or self.roster_hash
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
        # Tests may inject _fs; production uses a thread-local HfFileSystem so
        # one worker's remote session/cache cannot serialize every other worker.
        self._fs = None
        self._fs_tls = threading.local()
        self._s3_tls = threading.local()
        self.remote_streams = max(
            1,
            int(os.environ.get("SNG_HF_STREAMS", min(DEFAULT_REMOTE_STREAMS, workers))),
        )
        self._remote_sem = threading.BoundedSemaphore(self.remote_streams)
        self.worker_state = [WorkerState() for _ in range(workers)]
        self.stop = threading.Event()
        self.planner_done = threading.Event()
        self.in_flight_bytes = 0
        self.failed_shards = 0
        # terminally-failed shards per family (session-local, like the planner's
        # dispatched counter): subtracted from in-flight so a dead source can't
        # look like it is holding its blend share
        self._family_failed: dict[str, int] = {}
        self._source_failed: dict[str, int] = {}
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
        self._preflight_done = False
        # checkpoint status, surfaced in the dashboard header (not the event tail)
        self.last_checkpoint_at: float | None = None
        self.checkpoints_written = 0
        self.disk_free = 0
        self._last_memory_trim = 0.0
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

    def _family_progress(self) -> dict[str, dict[str, int]]:
        with self._merge_lock:
            family_bytes = dict(self.state.family_bytes)
            family_done = dict(self.state.family_done)
            source_bytes = dict(self.state.source_bytes)
            source_done = dict(self.state.source_done)
        with self._lock:
            family_failed = dict(self._family_failed)
        return {
            f.id: {
                "bytes": int(family_bytes.get(f.id, 0)),
                "done": int(family_done.get(f.id, 0)),
                "failed": int(family_failed.get(f.id, 0)),
                "sources": {
                    s.id: {
                        "bytes": int(source_bytes.get(s.id, 0)),
                        "done": int(source_done.get(s.id, 0)),
                        "cap": int(s.cap_bytes or 0),
                    }
                    for s in f.sources
                },
            }
            for f in self.families
        }

    def _cap_mode(self) -> bool:
        return any(f.cap_bytes is not None for f in self.families)

    def _family_cap(self, family: str) -> int | None:
        return self._families[family].cap_bytes

    def _family_inflight(
        self,
        fid: str,
        completed: dict[str, int],
        dispatched: dict[str, int],
        failed: dict[str, int],
    ) -> int:
        return max(dispatched.get(fid, 0) - completed.get(fid, 0) - failed.get(fid, 0), 0)

    def _family_dispatchable(
        self,
        fid: str,
        counted: dict[str, int],
        completed: dict[str, int],
        dispatched: dict[str, int],
        failed: dict[str, int],
        est: dict[str, float],
    ) -> bool:
        cap = self._family_cap(fid)
        if cap is None:
            return True
        if counted.get(fid, 0) >= cap:
            return False
        if completed.get(fid, 0) == 0 and self._family_inflight(fid, completed, dispatched, failed):
            return False
        return est.get(fid, 0.0) < cap

    def _family_waiting_on_inflight(
        self,
        fid: str,
        counted: dict[str, int],
        completed: dict[str, int],
        dispatched: dict[str, int],
        failed: dict[str, int],
    ) -> bool:
        cap = self._family_cap(fid)
        return (
            cap is not None
            and counted.get(fid, 0) < cap
            and self._family_inflight(fid, completed, dispatched, failed) > 0
        )

    def _source_inflight(
        self,
        sid: str,
        completed: dict[str, int],
        dispatched: dict[str, int],
        failed: dict[str, int],
    ) -> int:
        return max(dispatched.get(sid, 0) - completed.get(sid, 0) - failed.get(sid, 0), 0)

    def _source_dispatchable(
        self,
        sid: str,
        counted: dict[str, int],
        completed: dict[str, int],
        dispatched: dict[str, int],
        failed: dict[str, int],
        est: dict[str, float],
    ) -> bool:
        cap = self._source_caps.get(sid)
        if cap is None:
            return True
        if counted.get(sid, 0) >= cap:
            return False
        if completed.get(sid, 0) == 0 and self._source_inflight(sid, completed, dispatched, failed):
            return False
        return est.get(sid, 0.0) < cap

    def _source_waiting_on_inflight(
        self,
        sid: str,
        counted: dict[str, int],
        completed: dict[str, int],
        dispatched: dict[str, int],
        failed: dict[str, int],
    ) -> bool:
        cap = self._source_caps.get(sid)
        return (
            cap is not None
            and counted.get(sid, 0) < cap
            and self._source_inflight(sid, completed, dispatched, failed) > 0
        )

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
            source_order = [s.id for f in self.families for s in f.sources]
            ahead = {fid: next(gens[fid], None) for fid in order}
            # seed from restored completed counts so the in-flight estimate is
            # live immediately after a resume (not clamped to 0 — see the fn)
            dispatched = resume_dispatched(self.state.family_done, order)
            source_dispatched = resume_dispatched(self.state.source_done, source_order)
            for fid in order:
                if ahead[fid] is None:
                    self.events.log("family_done", family=fid)
            while not self.stop.is_set():
                with self._merge_lock:
                    counted = dict(self.state.family_bytes)
                    completed = dict(self.state.family_done)
                    source_counted = dict(self.state.source_bytes)
                    source_completed = dict(self.state.source_done)
                with self._lock:
                    failed = dict(self._family_failed)
                    source_failed = dict(self._source_failed)
                est = estimated_family_bytes(counted, completed, dispatched, failed)
                source_est = estimated_family_bytes(
                    source_counted, source_completed, source_dispatched, source_failed
                )
                for fid in order:
                    while ahead.get(fid) is not None:
                        sid = ahead[fid].source.id
                        if source_counted.get(sid, 0) < (self._source_caps.get(sid) or 10**100):
                            break
                        self.events.log("source_cap_reached", source=sid)
                        ahead[fid] = next(gens[fid], None)
                    if ahead.get(fid) is None:
                        self.events.log("family_done", family=fid)
                live = [
                    fid for fid in order
                    if ahead.get(fid) is not None
                    and (
                        not self._cap_mode()
                        or self._family_dispatchable(
                            fid, counted, completed, dispatched, failed, est
                        )
                    )
                    and self._source_dispatchable(
                        ahead[fid].source.id,
                        source_counted,
                        source_completed,
                        source_dispatched,
                        source_failed,
                        source_est,
                    )
                ]
                if not live:
                    waiting = any(
                        ahead.get(fid) is not None
                        and (
                            self._family_waiting_on_inflight(
                                fid, counted, completed, dispatched, failed
                            )
                            or self._source_waiting_on_inflight(
                                ahead[fid].source.id,
                                source_counted,
                                source_completed,
                                source_dispatched,
                                source_failed,
                            )
                        )
                        for fid in order
                    )
                    if waiting:
                        self.stop.wait(0.25)
                        continue
                    break
                fid = pick_family(live, weights, est)
                task = ahead[fid]
                ahead[fid] = next(gens[fid], None)  # refill the prefetch slot
                if ahead[fid] is None:
                    self.events.log("family_done", family=fid)
                dispatched[fid] += 1  # count it now so the next pick sees in-flight
                source_dispatched[task.source.id] += 1
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
        active = [self._source_tasks(source) for source in family.sources]
        while active and not self.stop.is_set():
            next_active = []
            for gen in active:
                if self.stop.is_set():
                    return
                try:
                    yield next(gen)
                    next_active.append(gen)
                except StopIteration:
                    continue
            active = next_active

    def _source_tasks(self, source: Source):
        if self.stop.is_set():
            return
        n = self._resolve_source(source)
        if n is None:
            return
        rev = self._source_revision(source)
        for shard in range(n):
            if self.stop.is_set():
                return
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
                        error_kind=kind, error=err_text(e), **error_debug_fields(e),
                    )
                    return None
                self.events.log(
                    "warn", stage="plan", source=source.id, error_kind=kind,
                    retry_in_s=round(delay), error=err_text(e),
                    **error_debug_fields(e),
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
            if "://" not in files:
                urls = sorted(glob.glob(files))
                if not urls:
                    raise FileNotFoundError(f"{source.id}: no files matched {files!r}")
                with self._ds_lock:
                    self._ds_cache[source.id] = urls
                return urls
            if rev and "hf://datasets/" in files:
                head, rest = files.split("hf://datasets/", 1)
                org, repo_name, tail = rest.split("/", 2)
                files = f"{head}hf://datasets/{org}/{repo_name}@{rev}/{tail}"
            if source.format != "parquet" and files.startswith("hf://"):
                urls = self._glob_hf_files(files)
                with self._ds_lock:
                    self._ds_cache[source.id] = urls
                return urls
            ds = load_dataset(
                source.format, data_files=files, split="train",
                streaming=True, token=self.token,
            )
        elif source.format == "swh":
            ds = load_dataset(
                source.repo, split="train",
                streaming=True, token=self.token, revision=rev,
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

    def _remote_fs(self):
        if self._fs is not None:
            return self._fs
        fs = getattr(self._fs_tls, "fs", None)
        if fs is None:
            from huggingface_hub import HfFileSystem

            fs = HfFileSystem(token=self.token)
            self._fs_tls.fs = fs
        return fs

    def _glob_hf_files(self, pattern: str) -> list[str]:
        return [
            p if p.startswith("hf://") else f"hf://{p}"
            for p in sorted(self._remote_fs().glob(pattern))
        ]

    def _source_shards(self, source: Source) -> int:
        n = len(self._load_source(source))
        self.events.log("source", source=source.id, shards=n)
        return int(n)

    def preflight_sources(self) -> None:
        """Resolve every configured source and verify the text column up front.

        The training run is multi-day; access, schema, or permission failures
        must happen before counting starts, not after a bucket reaches a source
        days later. This warms the URL cache and pins dataset revisions.
        """
        if self._preflight_done:
            return
        checked = 0
        self.events.log("preflight_start")
        for family in self.families:
            for source in family.sources:
                try:
                    urls = self._load_source(source)
                    if not urls:
                        raise RuntimeError("no shards resolved")
                    self._preflight_source_schema(source, urls[0])
                except Exception as e:  # noqa: BLE001 - fail before counting anything
                    self.events.log(
                        "error", stage="preflight", source=source.id,
                        error=err_text(e), **error_debug_fields(e),
                    )
                    raise RuntimeError(f"preflight failed for {source.id}: {e}") from e
                checked += 1
                self.events.log(
                    "preflight_source",
                    family=family.id,
                    source=source.id,
                    repo=source.repo,
                    config=source.config,
                    format=source.format,
                    text_field=source.text_field,
                    cap_bytes=source.cap_bytes,
                    shards=len(urls),
                )
        self._preflight_done = True
        self.events.log("preflight_done", sources=checked)

    def _preflight_source_schema(self, source: Source, url: str) -> None:
        if source.format == "swh":
            required = set(source.metadata_fields or ())
            if not required:
                raise ValueError(f"{source.id}: missing SWH metadata field contract")
            pf, fh = self._open_parquet(url)
            try:
                names = set(pf.schema_arrow.names)
                missing = sorted(required - names)
                if missing:
                    raise ValueError(
                        f"{source.id}: missing metadata columns {missing}; "
                        f"columns={pf.schema_arrow.names}"
                    )
            finally:
                if fh is not None:
                    fh.close()
            return

        if source.format == "json":
            import gzip
            import json

            with self._open_raw(url) as fh:
                gz = gzip.GzipFile(fileobj=fh) if url.endswith(".gz") else fh
                obj = json.loads(gz.readline())
            if source.text_field not in obj:
                raise ValueError(
                    f"{source.id}: missing {source.text_field!r}; "
                    f"columns={sorted(obj)}"
                )
            return

        if "://" not in url:
            import pyarrow.parquet as pq

            pf = pq.ParquetFile(url, pre_buffer=False)
            if source.text_field not in pf.schema_arrow.names:
                raise ValueError(
                    f"{source.id}: missing {source.text_field!r}; "
                    f"columns={pf.schema_arrow.names}"
                )
            return

        pf, fh = self._open_parquet(url)
        try:
            if source.text_field not in pf.schema_arrow.names:
                raise ValueError(
                    f"{source.id}: missing {source.text_field!r}; "
                    f"columns={pf.schema_arrow.names}"
                )
        finally:
            if fh is not None:
                fh.close()

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
            fh = self._remote_fs().open(
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

    def _mark_source_failed(self, source: str) -> None:
        with self._lock:
            self._source_failed[source] = self._source_failed.get(source, 0) + 1

    def _acquire_remote_stream(self, url: str, ws: WorkerState, sid: str) -> bool:
        if "://" not in url:
            return False
        while not self.stop.is_set():
            ws.task = f"{sid} (waiting remote stream)"
            ws.last_progress = time.monotonic()
            if self._remote_sem.acquire(timeout=1.0):
                ws.task = sid
                ws.last_progress = time.monotonic()
                return True
        return False

    def _release_remote_stream(self, acquired: bool) -> None:
        if acquired:
            self._remote_sem.release()

    def _note_worker_progress(self, ws: WorkerState, sid: str) -> None:
        now = time.monotonic()
        gap = now - ws.last_progress
        if gap >= READ_GAP_WARN_S and now - ws.last_gap_log >= READ_GAP_WARN_S:
            ws.last_gap_log = now
            self.events.log(
                "warn",
                stage="read_gap",
                shard=sid,
                silent_s=round(gap),
                shard_bytes=ws.shard_bytes,
            )
        ws.last_progress = now

    def _remaining_cap_for_worker(self, task: ShardTask, ws: WorkerState) -> int | None:
        with self._merge_lock:
            remaining = self._remaining_cap_locked(task)
        if remaining is None:
            return None
        return max(remaining - ws.shard_bytes, 0)

    def _count_arrow_with_cap(self, batch, remaining: int | None):
        """Count a whole Arrow batch, or only its largest row prefix that fits.

        The prefix path only runs on the final boundary batch for a capped
        source/family, so a tiny Python pass is cheaper than either overshooting
        a hard cap or discarding a large shard wholesale.
        """
        tally = sngram.LocalTally()
        n = tally.count_arrow(batch)
        if remaining is None or n <= remaining:
            return tally, n, False
        if remaining <= 0:
            return sngram.LocalTally(), 0, True

        import pyarrow as pa

        values = _prefix_values_to_bytes(batch.column(0).to_pylist(), remaining)
        if not values:
            return sngram.LocalTally(), 0, True
        name = batch.schema.names[0]
        prefix = pa.table({name: pa.array(values, type=batch.column(0).type)})
        prefix_tally = sngram.LocalTally()
        prefix_n = prefix_tally.count_arrow(prefix)
        return prefix_tally, prefix_n, True

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
                self._clear_stall(wid, ws)
                ws.task = "idle"

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
        ws.stalled = False
        ws.stall_started = 0.0
        ws.stall_count = 0
        ws.max_silent_s = 0.0
        ws.last_gap_log = 0.0
        if task.source.format == "swh":
            self._run_swh_shard(ws, task, sid)
            return
        if task.source.format == "json":
            self._run_json_shard(ws, task, sid)
            return
        while not self.stop.is_set():
            remote_acquired = False
            try:
                url = self._load_source(task.source)[task.shard]
                remote_acquired = self._acquire_remote_stream(url, ws, sid)
                if self.stop.is_set():
                    self._drop_in_flight(ws)
                    return
                fh = None
                try:
                    pf, fh = self._open_parquet(url, ws)
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
                    cap_stop = False
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
                            remaining = self._remaining_cap_for_worker(task, ws)
                            batch_tally, n, capped = self._count_arrow_with_cap(
                                batch, remaining
                            )
                            uncommitted += n
                            ws.shard_bytes += n
                            self._note_worker_progress(ws, sid)
                            with self._lock:
                                self.in_flight_bytes += n
                            if n:
                                rg_tally.add_from(batch_tally)
                            if capped:
                                cap_stop = True
                                break
                        # the row group streamed cleanly: fold it in and advance
                        # the cursor so a later failure never re-reads it
                        file_tally.add_from(rg_tally)
                        uncommitted = 0
                        next_rg = rg + 1
                        if cap_stop:
                            break
                finally:
                    if fh is not None:
                        fh.close()
                    self._release_remote_stream(remote_acquired)
                    # hand this file's row-group buffers back to the OS so RSS
                    # resets between shards instead of creeping over a long run
                    _release_arrow_pool()
                    self._maybe_trim_memory("shard", sid)
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
                        retries=task.retries, retry_in_s=round(delay),
                        error=err_text(e), **error_debug_fields(e),
                    )
                ws.task = f"{sid} (retry in {delay:.0f}s)"
                self.stop.wait(delay)  # interruptible backoff
                delay = min(delay * 2, RETRY_CAP_S)
                continue

            self._commit_tally(ws, task, sid, file_tally, ws.shard_bytes)
            return

    def _open_raw(self, url: str, ws: WorkerState | None = None):
        if "://" in url:
            fh = self._remote_fs().open(
                url, mode="rb",
                cache_type="none", block_size=0,
            )
            if ws is not None:
                return _HeartbeatFile(fh, ws)
            return fh
        fh = open(url, "rb")
        return _HeartbeatFile(fh, ws) if ws is not None else fh

    def _s3_client(self):
        client = getattr(self._s3_tls, "client", None)
        if client is None:
            try:
                import boto3
            except ImportError as e:  # pragma: no cover - exercised on run hosts
                raise RuntimeError(
                    "Stack v2 SWH content fetch needs boto3; install sngram[train]"
                ) from e
            client = boto3.Session().client("s3")
            self._s3_tls.client = client
        return client

    def _swh_content_url(self, source: Source, blob_id: str) -> str:
        if not source.content_prefix:
            raise ValueError(f"{source.id}: SWH source is missing content_prefix")
        return f"{source.content_prefix.rstrip('/')}/{blob_id}"

    def _read_swh_content_once(self, url: str) -> bytes:
        if url.startswith("file://"):
            parsed = urlparse(url)
            with gzip.open(parsed.path, "rb") as fh:
                return fh.read()
        if "://" not in url:
            with gzip.open(url, "rb") as fh:
                return fh.read()

        try:
            from smart_open import open as smart_open
        except ImportError as e:  # pragma: no cover - exercised on run hosts
            raise RuntimeError(
                "Stack v2 SWH content fetch needs smart_open[s3]; install sngram[train]"
            ) from e

        transport_params = {}
        if url.startswith("s3://"):
            transport_params["client"] = self._s3_client()
        with smart_open(
            url, "rb", compression=".gz", transport_params=transport_params
        ) as fh:
            return fh.read()

    def _read_swh_content(self, url: str, ws: WorkerState, sid: str) -> tuple[bytes, float]:
        delay = 0.5
        for attempt in range(1, SWH_OBJECT_ATTEMPTS + 1):
            start = time.monotonic()
            try:
                data = self._read_swh_content_once(url)
                ws.last_progress = time.monotonic()
                return data, ws.last_progress - start
            except Exception as e:  # noqa: BLE001 - classified below
                kind = classify_error(e)
                if kind == "transient" and attempt < SWH_OBJECT_ATTEMPTS:
                    self.events.log(
                        "warn",
                        stage="s3_object",
                        shard=sid,
                        url=url,
                        error_kind=kind,
                        attempt=attempt,
                        retry_in_s=round(delay, 1),
                        error=err_text(e),
                        **error_debug_fields(e),
                    )
                    self.stop.wait(delay)
                    delay = min(delay * 2, RETRY_CAP_S)
                    continue
                raise
        raise RuntimeError("unreachable SWH retry state")

    def _run_swh_shard(self, ws: WorkerState, task: ShardTask, sid: str) -> None:
        import pyarrow as pa

        delay = RETRY_BASE_S
        while not self.stop.is_set():
            file_tally = sngram.LocalTally()
            ws.shard_bytes = 0
            remote_acquired = False
            bucket = task.source.config or task.source.family
            stats: dict[str, int] = {
                "scanned_rows": 0,
                "accepted_objects": 0,
                "accepted_bytes": 0,
                "decoded_bytes": 0,
                "fetched_bytes": 0,
                "fetch_errors": 0,
                "decode_errors": 0,
            }
            latencies: list[float] = []
            texts: list[str] = []
            text_bytes = 0
            started = time.monotonic()
            try:
                url = self._load_source(task.source)[task.shard]
                self.events.log(
                    "swh_manifest_start",
                    shard=sid,
                    source=task.source.id,
                    bucket=bucket,
                    url=url,
                    revision=task.revision,
                    content_prefix=task.source.content_prefix,
                    metadata_fields=list(task.source.metadata_fields or ()),
                )
                remote_acquired = self._acquire_remote_stream(url, ws, sid)
                if self.stop.is_set():
                    self._drop_in_flight(ws)
                    return
                fh = None

                def flush() -> bool:
                    nonlocal text_bytes
                    if not texts:
                        return False
                    tbl = pa.table({"content": pa.array(texts, type=pa.large_string())})
                    remaining = self._remaining_cap_for_worker(task, ws)
                    batch_tally, n, capped = self._count_arrow_with_cap(tbl, remaining)
                    if n:
                        file_tally.add_from(batch_tally)
                    ws.shard_bytes += n
                    stats["accepted_bytes"] += n
                    self._note_worker_progress(ws, sid)
                    with self._lock:
                        self.in_flight_bytes += n
                    texts.clear()
                    text_bytes = 0
                    return capped

                cap_stop = False
                try:
                    pf, fh = self._open_parquet(url, ws)
                    fields = list(task.source.metadata_fields or ())
                    names = pf.schema_arrow.names
                    missing = [field for field in fields if field not in names]
                    if missing:
                        raise ValueError(
                            f"{task.source.id}: missing metadata columns {missing}; "
                            f"columns={names}"
                        )
                    for batch in pf.iter_batches(
                        batch_size=SWH_METADATA_BATCH_ROWS, columns=fields
                    ):
                        if self.stop.is_set():
                            self._drop_in_flight(ws)
                            return
                        for row in batch.to_pylist():
                            stats["scanned_rows"] += 1
                            reason = stack_v2_skip_reason(row)
                            if reason:
                                _stats_skip(stats, reason)
                                continue
                            row_bucket = stack_v2_bucket_for(
                                row.get("language"),
                                row.get("extension"),
                                row.get("path"),
                            )
                            if row_bucket != bucket:
                                _stats_skip(stats, "bucket")
                                continue
                            if self._remaining_cap_for_worker(task, ws) == 0:
                                cap_stop = True
                                break
                            blob_id = str(row["blob_id"])
                            url = self._swh_content_url(task.source, blob_id)
                            try:
                                raw, elapsed = self._read_swh_content(url, ws, sid)
                                latencies.append(elapsed)
                                if elapsed >= SWH_SLOW_OBJECT_S:
                                    self.events.log(
                                        "s3_slow_object",
                                        shard=sid,
                                        source=task.source.id,
                                        bucket=bucket,
                                        blob_id=blob_id,
                                        elapsed_ms=int(elapsed * 1000),
                                    )
                            except Exception as e:  # noqa: BLE001
                                stats["fetch_errors"] += 1
                                self.events.log(
                                    "s3_object_error",
                                    shard=sid,
                                    source=task.source.id,
                                    bucket=bucket,
                                    blob_id=blob_id,
                                    error_kind=classify_error(e),
                                    stage="fetch",
                                    error=err_text(e),
                                    **error_debug_fields(e),
                                )
                                continue
                            stats["fetched_bytes"] += len(raw)
                            try:
                                text = raw.decode(str(row["src_encoding"]))
                            except Exception as e:  # noqa: BLE001
                                stats["decode_errors"] += 1
                                self.events.log(
                                    "s3_object_error",
                                    shard=sid,
                                    source=task.source.id,
                                    bucket=bucket,
                                    blob_id=blob_id,
                                    error_kind="decode",
                                    stage="decode",
                                    encoding=row.get("src_encoding"),
                                    error=err_text(e),
                                )
                                continue
                            if not text:
                                _stats_skip(stats, "empty_content")
                                continue
                            encoded_len = len(text.encode("utf-8"))
                            stats["decoded_bytes"] += encoded_len
                            texts.append(text)
                            text_bytes += encoded_len
                            stats["accepted_objects"] += 1
                            if (
                                len(texts) >= SWH_TEXT_BATCH_ROWS
                                or text_bytes >= SWH_TEXT_BATCH_BYTES
                            ):
                                cap_stop = flush()
                                if cap_stop:
                                    break
                        if cap_stop:
                            break
                    if not cap_stop:
                        flush()
                finally:
                    if fh is not None:
                        fh.close()
                    self._release_remote_stream(remote_acquired)
                    remote_acquired = False
                    _release_arrow_pool()
                    self._maybe_trim_memory("swh_shard", sid)
            except Exception as e:  # noqa: BLE001 - metadata shard read failure
                self._release_remote_stream(remote_acquired)
                self._drop_in_flight(ws)
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
                if task.retries <= 3 or task.retries % 10 == 0:
                    self.events.log(
                        "warn", stage="swh_shard", shard=sid, error_kind=kind,
                        retries=task.retries, retry_in_s=round(delay),
                        error=err_text(e), **error_debug_fields(e),
                    )
                ws.task = f"{sid} (retry in {delay:.0f}s)"
                self.stop.wait(delay)
                delay = min(delay * 2, RETRY_CAP_S)
                continue

            secs = time.monotonic() - started
            self.events.log(
                "s3_batch",
                shard=sid,
                source=task.source.id,
                bucket=bucket,
                secs=round(secs, 1),
                latency_p50_ms=_latency_ms(latencies, 0.50),
                latency_p95_ms=_latency_ms(latencies, 0.95),
                latency_max_ms=_latency_ms(latencies, 1.0),
                **stats,
            )
            self.events.log(
                "swh_bucket_progress",
                shard=sid,
                source=task.source.id,
                bucket=bucket,
                accepted_bytes=stats["accepted_bytes"],
                accepted_objects=stats["accepted_objects"],
                scanned_rows=stats["scanned_rows"],
                decoded_bytes=stats["decoded_bytes"],
                fetched_bytes=stats["fetched_bytes"],
                skips={
                    k.removeprefix("skipped_"): v
                    for k, v in stats.items()
                    if k.startswith("skipped_")
                },
                fetch_errors=stats["fetch_errors"],
                decode_errors=stats["decode_errors"],
            )
            self.events.log(
                "swh_manifest_done",
                shard=sid,
                source=task.source.id,
                bucket=bucket,
                rows=stats["scanned_rows"],
                secs=round(secs, 1),
            )
            self._commit_tally(ws, task, sid, file_tally, ws.shard_bytes)
            return

    def _run_json_shard(self, ws: WorkerState, task: ShardTask, sid: str) -> None:
        import gzip
        import json

        import pyarrow as pa

        delay = RETRY_BASE_S
        while not self.stop.is_set():
            file_tally = sngram.LocalTally()
            ws.shard_bytes = 0
            remote_acquired = False
            try:
                url = self._load_source(task.source)[task.shard]
                remote_acquired = self._acquire_remote_stream(url, ws, sid)
                if self.stop.is_set():
                    self._drop_in_flight(ws)
                    return
                with self._open_raw(url, ws) as fh:
                    gz = gzip.GzipFile(fileobj=fh) if url.endswith(".gz") else fh
                    batch: list[str] = []
                    batch_bytes = 0

                    cap_stop = False

                    def flush() -> bool:
                        nonlocal batch_bytes
                        if not batch:
                            return False
                        tbl = pa.table(
                            {task.source.text_field: pa.array(batch, type=pa.large_string())}
                        )
                        remaining = self._remaining_cap_for_worker(task, ws)
                        batch_tally, n, capped = self._count_arrow_with_cap(tbl, remaining)
                        if n:
                            file_tally.add_from(batch_tally)
                        ws.shard_bytes += n
                        self._note_worker_progress(ws, sid)
                        with self._lock:
                            self.in_flight_bytes += n
                        batch.clear()
                        batch_bytes = 0
                        return capped

                    for line in gz:
                        if self.stop.is_set():
                            self._drop_in_flight(ws)
                            return
                        obj = json.loads(line)
                        if task.source.text_field not in obj:
                            raise ValueError(
                                f"column [{task.source.text_field!r}] not in json object. "
                                f"columns in the object: {sorted(obj)}."
                            )
                        value = obj[task.source.text_field]
                        if isinstance(value, str):
                            batch.append(value)
                            batch_bytes += len(value.encode("utf-8"))
                        if len(batch) >= JSON_BATCH_ROWS or batch_bytes >= JSON_BATCH_BYTES:
                            cap_stop = flush()
                            if cap_stop:
                                break
                    if not cap_stop:
                        flush()
                self._release_remote_stream(remote_acquired)
                remote_acquired = False
                _release_arrow_pool()
                self._maybe_trim_memory("shard", sid)
            except Exception as e:  # noqa: BLE001 - classified below, never fatal
                self._release_remote_stream(remote_acquired)
                self._drop_in_flight(ws)
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
                if task.retries <= 3 or task.retries % 10 == 0:
                    self.events.log(
                        "warn", stage="shard", shard=sid, error_kind=kind,
                        retries=task.retries, retry_in_s=round(delay),
                        error=err_text(e), **error_debug_fields(e),
                    )
                ws.task = f"{sid} (retry in {delay:.0f}s)"
                self.stop.wait(delay)
                delay = min(delay * 2, RETRY_CAP_S)
                continue

            self._commit_tally(ws, task, sid, file_tally, ws.shard_bytes)
            return

    def _commit_tally(
        self, ws: WorkerState, task: ShardTask, sid: str, file_tally, shard_bytes: int
    ) -> None:
        # exactly-once: merge only after a shard streamed cleanly, under the
        # merge lock so checkpoints/mints see a consistent cut.
        skipped_for_cap = False
        with self._merge_lock:
            fam = task.source.family
            remaining = self._remaining_cap_locked(task)
            self.state.mark_done(task.source.id, task.n_shards, task.shard, task.revision)
            if remaining is not None and shard_bytes > remaining:
                skipped_for_cap = True
            else:
                self.counter.merge(file_tally)
                self.counter.add_files(1)
                self.state.family_bytes[fam] = (
                    self.state.family_bytes.get(fam, 0) + shard_bytes
                )
                self.state.family_done[fam] = self.state.family_done.get(fam, 0) + 1
                self.state.source_bytes[task.source.id] = (
                    self.state.source_bytes.get(task.source.id, 0) + shard_bytes
                )
                self.state.source_done[task.source.id] = (
                    self.state.source_done.get(task.source.id, 0) + 1
                )
        self._drop_in_flight(ws)
        task.accounted = True
        if skipped_for_cap:
            self._mark_family_failed(task.source.family)
            self._mark_source_failed(task.source.id)
            self.events.log(
                "cap_skip", shard=sid, bytes=shard_bytes,
                family=task.source.family, source=task.source.id,
            )
            return
        self.events.log(
            "shard",
            shard=sid,
            bytes=shard_bytes,
            secs=round(time.monotonic() - ws.started, 1),
            stall_count=ws.stall_count,
            max_silent_s=round(ws.max_silent_s),
        )

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
        self._mark_source_failed(task.source.id)
        task.accounted = True
        self.events.log(
            "error", stage="shard", shard=sid, error_kind=kind,
            error=err_text(e), **error_debug_fields(e),
        )

    def _drop_in_flight(self, ws: WorkerState) -> None:
        with self._lock:
            self.in_flight_bytes -= ws.shard_bytes
        ws.shard_bytes = 0

    def _maybe_trim_memory(self, stage: str, shard: str | None = None) -> None:
        before = rss_bytes()
        if before < MEMORY_SOFT_LIMIT_BYTES:
            return
        now = time.monotonic()
        if now - self._last_memory_trim < MEMORY_TRIM_INTERVAL_S:
            return
        self._last_memory_trim = now
        _release_arrow_pool()
        _collect_python_memory()
        trimmed = _trim_process_memory()
        after = rss_bytes()
        self.events.log(
            "memory_trim",
            stage=stage,
            shard=shard,
            rss_before=before,
            rss_after=after,
            trimmed=trimmed,
            memory=memory_stats(),
        )

    def _remaining_cap_locked(self, task: ShardTask) -> int | None:
        caps: list[int] = []
        if (cap := self._family_cap(task.source.family)) is not None:
            caps.append(cap - self.state.family_bytes.get(task.source.family, 0))
        if (cap := self._source_caps.get(task.source.id)) is not None:
            caps.append(cap - self.state.source_bytes.get(task.source.id, 0))
        if not caps:
            return None
        return max(min(caps), 0)

    # ---------------------------------------------------------- supervisor

    async def run(self) -> None:
        self.events.log(
            "run_start",
            schema=2,
            target=self.target,
            limit=self.limit,
            mint_every=self.mint_every,
            workers=self.workers,
            queue_depth=QUEUE_DEPTH_PER_WORKER * self.workers,
            batch_rows=BATCH_ROWS,
            json_batch_rows=JSON_BATCH_ROWS,
            json_batch_bytes=JSON_BATCH_BYTES,
            shard_cache_type=SHARD_CACHE_TYPE,
            shard_block_size=SHARD_BLOCK_SIZE,
            remote_streams=self.remote_streams,
            memory_soft_limit=MEMORY_SOFT_LIMIT_BYTES,
            memory_trim_interval_s=MEMORY_TRIM_INTERVAL_S,
            roster_hash=self.roster_hash,
            families=[
                {"id": f.id, "weight": f.weight, "sources": len(f.sources)}
                for f in self.families
            ],
        )
        try:
            self.preflight_sources()
        except Exception:
            self.events.close()
            raise
        loop = asyncio.get_running_loop()
        pool = ThreadPoolExecutor(max_workers=self.workers + 1, thread_name_prefix="sngram")
        futures = [pool.submit(self._plan)]
        futures += [pool.submit(self._worker, i) for i in range(self.workers)]

        last_ckpt = time.monotonic()
        finished = False
        short_exhausted = False
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
                    if self._cap_mode() and self.durable_bytes() < self.target:
                        short_exhausted = True
                        self._bump("errors")
                        self.events.log(
                            "error",
                            stage="exhausted_short",
                            bytes=self.durable_bytes(),
                            target=self.target,
                        )
                    finished = True
                    break
        finally:
            self.stop.set()
            pool.shutdown(wait=True, cancel_futures=True)
            for future in futures:
                if future.done() and (exc := future.exception()) is not None:
                    self.events.log("error", stage="thread", error=err_text(exc))
            self._mint_if_due()
            if finished and not short_exhausted:
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
                    memory=memory_stats(),
                    wall_s=round(time.monotonic() - self.started_at, 1),
                    families=self._family_progress(),
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
        self._maybe_trim_memory("checkpoint")
        # status lives in the header (live); the JSONL keeps the full beat as
        # debug material — it's split into small segments, not throttled
        self.last_checkpoint_at = time.monotonic()
        self.checkpoints_written += 1
        self.disk_free = free
        mem = memory_stats()
        self.events.log(
            "checkpoint", bytes=self.durable_bytes(), rss=mem["rss"],
            disk_free=free, in_flight_bytes=self.in_flight_bytes,
            memory=mem,
            families=self._family_progress(),
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
                ws.stall_started = now
                ws.stall_count += 1
                ws.max_silent_s = max(ws.max_silent_s, age)
                self.events.log(
                    "stall", worker=i, shard=ws.task, silent_s=round(age),
                    stall_count=ws.stall_count, shard_bytes=ws.shard_bytes,
                )
            elif age > STALL_AFTER_S:
                ws.max_silent_s = max(ws.max_silent_s, age)
            elif age <= STALL_AFTER_S:
                self._clear_stall(i, ws)

    def _clear_stall(self, wid: int, ws: WorkerState) -> None:
        if not ws.stalled:
            return
        now = time.monotonic()
        ws.stalled = False
        self.events.log(
            "stall_end",
            worker=wid,
            shard=ws.task,
            stalled_s=round(now - ws.stall_started),
            max_silent_s=round(ws.max_silent_s),
            stall_count=ws.stall_count,
            shard_bytes=ws.shard_bytes,
        )

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
