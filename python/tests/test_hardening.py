"""Production-readiness stress tests: the races, crashes, and storms a
10-day run will actually hit. Every test here encodes an invariant the
pipeline must keep under concurrency or failure, not just a happy path."""

import asyncio
import json
import threading
import time
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import sngram
from sngram.train import checkpoint
from sngram.train import pipeline as pl
from sngram.train.config import Family, Source
from sngram.train.events import EventLog
from sngram.train.pipeline import Trainer, classify_error, err_text
from sngram.train.units import parse_size

from test_pipeline import local_family, run_trainer, write_fixture  # noqa: E402


# ---------------------------------------------------------------- consistency


def test_checkpoint_is_consistent_cut_under_merge_storm(tmp_path: Path):
    """The C1/C2 race, hammered directly: workers merge+mark_done under the
    merge lock while a checkpointer saves/loads continuously. EVERY loaded
    checkpoint must be a consistent cut — the restored counter holds exactly
    the pairs of the shards the restored state records as done."""
    counter = sngram.BigramCounter()
    state = checkpoint.RunState()
    lock = threading.Lock()
    doc = b"ab" * 64  # 127 pairs per shard, deterministic
    pairs_per_shard = len(doc) - 1
    shards_per_thread = 200
    n_threads = 4
    stop = threading.Event()
    violations: list[str] = []

    def merger(tid: int) -> None:
        for i in range(shards_per_thread):
            staging = sngram.BigramCounter()
            staging.process(doc)
            with lock:
                counter.merge(staging)
                counter.add_files(1)
                state.mark_done(f"src-{tid}", shards_per_thread, i, "rev")
        # also exercise new-source insertion mid-save iteration
        with lock:
            state.mark_done(f"late-{tid}", 1, 0, None)

    def checkpointer() -> None:
        while not stop.is_set():
            with lock:
                checkpoint.save(tmp_path, counter, state)
            fresh = sngram.BigramCounter()
            loaded = checkpoint.load(tmp_path, fresh)
            done = sum(
                len(e["_done_set"])
                for sid, e in loaded.completed.items()
                if sid.startswith("src-")
            )
            expected = done * pairs_per_shard
            if fresh.pairs_processed != expected + sum(
                len(e["_done_set"]) * 0
                for e in loaded.completed.values()
            ) and fresh.pairs_processed != expected:
                violations.append(
                    f"pairs={fresh.pairs_processed} but done-shards imply {expected}"
                )

    threads = [threading.Thread(target=merger, args=(t,)) for t in range(n_threads)]
    ck = threading.Thread(target=checkpointer)
    ck.start()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    stop.set()
    ck.join()

    assert not violations, violations[:3]
    assert counter.pairs_processed == n_threads * shards_per_thread * pairs_per_shard


def test_interrupted_run_resumes_to_identical_table(tmp_path: Path):
    """Kill mid-run (limit), resume to completion: the final minted table must
    be byte-identical to an uninterrupted run — no double counts, no skips."""
    rows = ["fn main() { let x = foo_bar(42); }" + str(i) for i in range(50)]
    files = 12

    ref_dir = tmp_path / "ref"
    fam_ref = local_family(ref_dir, "alpha", rows, files=files)
    reference = run_trainer(ref_dir, [fam_ref])
    ref_table = (ref_dir / "bins" / "final_weights.bin").read_bytes()

    run_dir = tmp_path / "run"
    fam = local_family(run_dir, "alpha", rows, files=files)
    per_file = sum(len(r) for r in rows)
    first = run_trainer(run_dir, [fam], limit=per_file * 3)  # stops ~3 files in
    assert 0 < first.counter.files_processed < files, "must actually interrupt"

    second = run_trainer(run_dir, [fam], resume=True)
    assert second.counter.files_processed == files
    assert second.durable_bytes() == reference.durable_bytes()
    final = (run_dir / "bins" / "final_weights.bin").read_bytes()
    assert final == ref_table, "resumed run must mint the identical table"


def test_double_resume_changes_nothing(tmp_path: Path):
    """Resuming a finished run twice is a no-op both times (idempotence)."""
    fam = local_family(tmp_path, "alpha", ["hello world"] * 20, files=4)
    first = run_trainer(tmp_path, [fam])
    table_1 = (tmp_path / "bins" / "final_weights.bin").read_bytes()
    for _ in range(2):
        again = run_trainer(tmp_path, [fam], resume=True)
        assert again.durable_bytes() == first.durable_bytes()
        assert again.counter.files_processed == 4
    assert (tmp_path / "bins" / "final_weights.bin").read_bytes() == table_1


# ------------------------------------------------------------ failure storms


def make_trainer(tmp_path: Path, families, **kw) -> Trainer:
    defaults = dict(
        mint_dir=tmp_path / "bins",
        target=parse_size("1GB"),
        mint_every=parse_size("1GB"),
        workers=2,
        limit=None,
        checkpoint_every_s=3600.0,
        resume=False,
    )
    defaults.update(kw)
    return Trainer(families=families, **defaults)


def test_transient_errors_retry_forever_and_recover(tmp_path: Path, monkeypatch):
    """Two consecutive connection failures on a shard must not fail it: the
    worker backs off and the shard completes with full counts."""
    monkeypatch.setattr(pl, "RETRY_BASE_S", 0.01)
    monkeypatch.setattr(pl, "RETRY_CAP_S", 0.02)
    fam = local_family(tmp_path, "alpha", ["hello world"] * 10, files=2)
    trainer = make_trainer(tmp_path, [fam])
    trainer.preflight_sources()

    real = trainer._open_parquet
    failures = {"n": 0}

    def flaky(url, ws=None):
        if ws is not None and failures["n"] < 5:
            failures["n"] += 1
            raise ConnectionResetError("connection reset by peer")
        return real(url, ws)

    monkeypatch.setattr(trainer, "_open_parquet", flaky)
    asyncio.run(trainer.run())

    assert failures["n"] == 5, "the flaky path must actually fire"
    assert trainer.failed_shards == 0
    assert trainer.counter.files_processed == 2
    assert trainer.durable_bytes() == 2 * 10 * len("hello world")


def test_hard_errors_are_bounded_and_run_continues(tmp_path: Path, monkeypatch):
    monkeypatch.setattr(pl, "RETRY_BASE_S", 0.01)
    good = local_family(tmp_path, "good", ["abcdef"] * 5, files=1)
    bad = local_family(tmp_path, "bad", ["abcdef"] * 5, files=1)
    trainer = make_trainer(tmp_path, [good, bad])
    trainer.preflight_sources()

    real = trainer._open_parquet

    def fail_bad(url, ws=None):
        if ws is not None and "/bad-" in str(url):
            raise ValueError("worker-only hard failure")
        return real(url, ws)

    monkeypatch.setattr(trainer, "_open_parquet", fail_bad)
    asyncio.run(trainer.run())
    assert trainer.failed_shards == 1
    assert trainer.durable_bytes() == 5 * 6
    assert (tmp_path / "bins" / "final_weights.bin").exists()


def test_planner_transient_retry_is_capped_not_starving(tmp_path: Path, monkeypatch):
    """A perpetually-throttled source must be skipped after bounded retries so
    the planner can serve other families — and its shards stay unmarked."""
    monkeypatch.setattr(pl, "RETRY_BASE_S", 0.001)
    monkeypatch.setattr(pl, "RETRY_CAP_S", 0.002)
    good = local_family(tmp_path, "good", ["hello world"] * 10, files=1)
    throttled = local_family(tmp_path, "throttled", ["hello world"] * 10, files=1)
    trainer = make_trainer(tmp_path, [good, throttled])
    trainer.preflight_sources()

    real = trainer._source_shards

    def always_429(source):
        if source.family == "throttled":
            raise Exception("HTTP Error 429: Too Many Requests")
        return real(source)

    monkeypatch.setattr(trainer, "_source_shards", always_429)
    asyncio.run(trainer.run())

    assert trainer.durable_bytes() == 10 * len("hello world"), "good family unaffected"
    assert "throttled" not in trainer.state.completed, "skipped source stays unmarked"


def test_crash_restart_loop_resumes_and_finishes(tmp_path: Path, monkeypatch):
    """cli._run_until_done must survive a mid-run crash: rebuild from the
    checkpoint and complete, without minting 'final' from the crashed pass."""
    from sngram import cli

    monkeypatch.setattr(time, "sleep", lambda s: None)
    fam = local_family(tmp_path, "alpha", ["hello world"] * 20, files=4)
    calls = {"n": 0}

    def build(resume_now: bool) -> Trainer:
        calls["n"] += 1
        trainer = make_trainer(tmp_path, [fam], resume=resume_now)
        if calls["n"] == 1:
            real_checkpoint = trainer._checkpoint

            def exploding():
                real_checkpoint()
                if trainer.counter.files_processed >= 1:
                    raise RuntimeError("simulated supervisor crash")

            trainer._checkpoint = exploding
            trainer.checkpoint_every_s = 0.0  # checkpoint every tick
        return trainer

    trainer = cli._run_until_done(build, resume=False, dashboard=False)
    assert calls["n"] >= 2, "must have rebuilt after the crash"
    assert trainer.counter.files_processed == 4
    assert (tmp_path / "bins" / "final_weights.bin").exists()


def test_worker_survives_poison_batches(tmp_path: Path, monkeypatch):
    """Unexpected exceptions inside the worker loop must not kill the thread:
    remaining shards still complete."""
    monkeypatch.setattr(pl, "RETRY_BASE_S", 0.01)
    fam = local_family(tmp_path, "alpha", ["hello world"] * 10, files=3)
    trainer = make_trainer(tmp_path, [fam], workers=1)

    real = trainer._run_shard
    blown = {"n": 0}

    def sometimes_blows(ws, task):
        if task.shard == 1 and blown["n"] == 0:
            blown["n"] += 1
            raise RuntimeError("totally unexpected")
        return real(ws, task)

    monkeypatch.setattr(trainer, "_run_shard", sometimes_blows)
    asyncio.run(trainer.run())
    assert blown["n"] == 1
    # the blown shard was lost this pass but the worker survived for the rest
    assert trainer.counter.files_processed == 2
    # and a resume picks the lost shard back up
    second = make_trainer(tmp_path, [fam], resume=True)
    asyncio.run(second.run())
    assert second.counter.files_processed == 3


# --------------------------------------------------------------- supervision


def test_mint_storm_under_many_workers(tmp_path: Path):
    """16 workers racing dozens of tiny shards across many mint thresholds:
    labels unique and ordered, every minted file loads, counts exact."""
    rows = ["z" * 1000] * 50  # 50 KB per file
    fam = local_family(tmp_path, "alpha", rows, files=40)
    trainer = run_trainer(
        tmp_path,
        [fam],
        workers=16,
        target=parse_size("2MB"),
        mint_every=parse_size("100KB"),
    )
    labels = trainer.state.mints_done
    assert len(labels) == len(set(labels)), "no duplicate mint labels"
    for label in labels:
        table = sngram.WeightTable.from_path(tmp_path / "bins" / f"{label}_weights.bin")
        assert table.version == 1
    assert trainer.durable_bytes() == 40 * 50 * 1000


def test_watchdog_flags_and_clears_stalls(tmp_path: Path):
    fam = local_family(tmp_path, "alpha", ["x"], files=1)
    trainer = make_trainer(tmp_path, [fam])
    ws = trainer.worker_state[0]
    ws.task = "alpha#0"
    ws.last_progress = time.monotonic() - 10_000
    trainer._watchdog()
    assert ws.stalled
    stall = next(e for e in trainer.events.tail if e["kind"] == "stall")
    assert stall["stall_count"] == 1
    ws.last_progress = time.monotonic()
    trainer._watchdog()
    assert not ws.stalled
    end = next(e for e in trainer.events.tail if e["kind"] == "stall_end")
    assert end["stall_count"] == 1
    assert end["stalled_s"] >= 0
    trainer.events.close()


def test_dashboard_renders_through_a_live_run(tmp_path: Path):
    """render() runs 4x/s for 10 days; it must never throw, in any state."""
    from sngram.train.dashboard import render

    fam = local_family(tmp_path, "alpha", ["hello world"] * 50, files=6)
    renders = {"n": 0}

    def on_refresh(t):
        render(t)  # must not raise mid-run
        renders["n"] += 1

    trainer = make_trainer(tmp_path, [fam])
    trainer.on_refresh = on_refresh
    asyncio.run(trainer.run())
    render(trainer)  # and in the terminal state
    assert renders["n"] >= 1


# ------------------------------------------------------------ event log + io


def test_event_log_rotation_and_thread_safety(tmp_path: Path):
    path = tmp_path / "events.jsonl"
    log = EventLog(path, segment_bytes=20_000)

    def spam(tid: int):
        for i in range(500):
            log.log("shard", thread=tid, i=i, filler="x" * 50)

    threads = [threading.Thread(target=spam, args=(t,)) for t in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    log.close()

    segments = EventLog.segment_paths(path)
    assert len(segments) > 1, "rotation must have split the log into segments"
    total = 0
    for seg in segments:
        assert seg.stat().st_size <= 20_000 + 512, "each segment stays small"
        lines = seg.read_text().splitlines()
        for line in lines:
            json.loads(line)  # every line is intact JSON despite 8 writers
        total += len(lines)
    assert total == 8 * 500, "no event dropped across the split"


def test_legacy_v1_checkpoint_still_loads(tmp_path: Path):
    """The user's existing on-disk checkpoint (counts.bin + v1 state.json)
    must restore after the single-file migration."""
    c = sngram.BigramCounter()
    c.process(b"hello world")
    c.add_files(1)
    (tmp_path / "counts.bin").write_bytes(c.snapshot())
    (tmp_path / "state.json").write_text(
        json.dumps(
            {
                "pairs": c.pairs_processed,
                "bytes": c.bytes_processed,
                "files": c.files_processed,
                "completed": {"the-stack": {"n_shards": 5, "done": [0, 2]}},
                "mints_done": ["100gb"],
            }
        )
    )
    fresh = sngram.BigramCounter()
    state = checkpoint.load(tmp_path, fresh)
    assert fresh.bytes_processed == 11
    assert state.mints_done == ["100gb"]
    # v1 entries carry no revision: any pinned revision invalidates them,
    # which re-streams those shards — safe, never lossy
    assert state.is_done("the-stack", 5, 0, None)
    assert not state.is_done("the-stack", 5, 0, "somesha")


def test_restore_requires_fresh_counter(tmp_path: Path):
    c = sngram.BigramCounter()
    c.process(b"abc")
    checkpoint.save(tmp_path, c, checkpoint.RunState())
    with pytest.raises(ValueError, match="fresh"):
        checkpoint.load(tmp_path, c)


# ------------------------------------------------------------ classification


def test_classify_sees_through_wrapper_exceptions():
    inner = TimeoutError("read timed out")
    try:
        try:
            raise inner
        except TimeoutError as t:
            raise ValueError("An error occurred while generating the dataset") from t
    except ValueError as wrapper:
        assert classify_error(wrapper) == "transient"


def test_classify_modern_network_failures():
    for msg in (
        "ServerDisconnectedError: server disconnected",
        "ClientPayloadError: response payload is not completed",
        "SlowDown: please reduce your request rate",
        "ServiceUnavailable: try again",
        "ProtocolError: ('Connection aborted.', RemoteDisconnected(...))",
        "HTTP 500 InternalError",
    ):
        assert classify_error(Exception(msg)) == "transient", msg


def test_incomplete_body_byte_counts_are_not_missing():
    msg = (
        "RemoteProtocolError: peer closed connection without sending complete "
        "message body (received 404195355 bytes, expected 511313345)"
    )
    assert classify_error(Exception(msg)) == "transient"


def test_err_text_is_bounded():
    huge = Exception("x" * 100_000)
    assert len(err_text(huge)) <= 400


# ------------------------------------------------------------------ bindings


def test_concurrent_merge_and_snapshot_never_crashes():
    """GIL-released merges from many threads while snapshots run: the final
    state must be exact; intermediate snapshots must parse."""
    counter = sngram.BigramCounter()
    n_threads, per_thread = 8, 300
    doc = b"the quick brown fox"

    def merger():
        for _ in range(per_thread):
            t = sngram.BigramCounter()
            t.process(doc)
            counter.merge(t)

    def snapshotter():
        for _ in range(200):
            snap = counter.snapshot()
            assert len(snap) == 65_536 * 8

    threads = [threading.Thread(target=merger) for _ in range(n_threads)]
    threads.append(threading.Thread(target=snapshotter))
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert counter.pairs_processed == n_threads * per_thread * (len(doc) - 1)
    assert counter.count(ord("t"), ord("h")) == n_threads * per_thread  # one "th"


def test_load_source_resolves_parquet_file_list(tmp_path: Path, monkeypatch):
    """datasets does the resolution (config/revision/glob); we take the concrete
    file list and read it ourselves, so shard index == file index."""
    import datasets

    trainer = make_trainer(tmp_path, [])
    trainer.state.revisions["example/code-roster"] = "deadbeef"
    fake_files = [
        "hf://datasets/example/code-roster@deadbeef/data/000.parquet",
        "hf://datasets/example/code-roster@deadbeef/data/001.parquet",
    ]

    class FakeEx:
        kwargs = {"files": fake_files}

    class FakeDS:
        _ex_iterable = FakeEx()

    monkeypatch.setattr(datasets, "load_dataset", lambda *a, **k: FakeDS())
    src = Source("code-roster", "example/code-roster", "content")
    assert trainer._load_source(src) == fake_files
    assert trainer._source_shards(src) == 2
    trainer.events.close()


def test_shard_read_uses_bounded_readahead_cache(tmp_path: Path, monkeypatch):
    """Remote shard reads use bounded readahead: enough buffering to avoid HF/Xet
    range-request storms, but no whole-file eager read."""
    import io
    import pyarrow.parquet as pq

    trainer = make_trainer(tmp_path, [])
    captured: dict = {}

    class FakeFS:
        def open(self, url, mode="rb", **kw):
            captured.update(kw)
            captured["url"] = url
            return io.BytesIO(b"")

    trainer._fs = FakeFS()
    monkeypatch.setattr(
        pq, "ParquetFile",
        lambda fh, **kw: captured.__setitem__("pre_buffer", kw.get("pre_buffer")),
    )
    trainer._open_parquet("hf://datasets/x/y@sha/a.parquet")

    assert captured["cache_type"] == "readahead"
    assert captured["block_size"] == 64 * 1024 * 1024
    assert captured["pre_buffer"] is False, "no eager whole-file buffering"
    trainer.events.close()


def test_remote_json_reads_stream_without_readahead_cache(tmp_path: Path):
    """Remote JSON gzip shards are sequential streams, so they must not retain a
    64 MiB range cache per file like parquet readers do."""
    import io

    trainer = make_trainer(tmp_path, [])
    captured: dict = {}

    class FakeFS:
        def open(self, url, mode="rb", **kw):
            captured.update(kw)
            captured["url"] = url
            return io.BytesIO(b"")

    trainer._fs = FakeFS()
    fh = trainer._open_raw("hf://datasets/x/y@sha/github-dedup-000.json.gz")
    fh.close()

    assert captured["cache_type"] == "none"
    assert captured["block_size"] == 0
    trainer.events.close()


def test_heartbeat_raw_stream_close_releases_remote_state(tmp_path: Path):
    """Closing a heartbeat-wrapped remote stream closes streaming state eagerly
    instead of waiting for Python object destruction."""

    trainer = make_trainer(tmp_path, [])
    ws = pl.WorkerState()
    closed: list[str] = []

    class FakeResponse:
        def close(self):
            closed.append("response")

    class FakeExitStack:
        def close(self):
            closed.append("exit_stack")

    class FakeRemote:
        def __init__(self):
            self.response = FakeResponse()
            self._exit_stack = FakeExitStack()
            self._stream_buffer = bytearray(b"cached")
            self.closed = False

        def read(self, *args, **kwargs):
            return b""

        def close(self):
            self.closed = True
            closed.append("file")

    raw = FakeRemote()
    wrapped = pl._HeartbeatFile(raw, ws)
    wrapped.close()

    assert closed == ["response", "exit_stack", "file"]
    assert raw._stream_buffer == bytearray()
    assert wrapped._fh is None
    trainer.events.close()


def test_checkpoint_trims_memory_above_soft_limit(tmp_path: Path, monkeypatch):
    """A long run should actively return freed buffers once RSS crosses the
    training soft cap, and leave a log event proving it happened."""

    trainer = make_trainer(tmp_path, [])
    calls: list[str] = []
    gb = 10**9
    rss_values = iter([6 * gb, 4 * gb])

    monkeypatch.setattr(pl, "rss_bytes", lambda: next(rss_values, 4 * gb))
    monkeypatch.setattr(pl, "_release_arrow_pool", lambda: calls.append("arrow"))
    monkeypatch.setattr(pl, "_collect_python_memory", lambda: calls.append("gc"))
    monkeypatch.setattr(pl, "_trim_process_memory", lambda: calls.append("malloc") or True)

    trainer._checkpoint()

    events = [
        json.loads(line)
        for line in (tmp_path / "bins" / "train-events.jsonl").read_text().splitlines()
    ]
    trim = next(e for e in events if e["kind"] == "memory_trim")
    assert calls == ["arrow", "gc", "malloc"]
    assert trim["stage"] == "checkpoint"
    assert trim["rss_before"] == 6 * gb
    assert trim["rss_after"] == 4 * gb
    trainer.events.close()


def test_json_shard_attempts_memory_trim_after_close(tmp_path: Path, monkeypatch):
    """Sequential JSON shards should run the memory gate after the file closes,
    so a ballooning stream does not wait for the next checkpoint."""

    path = tmp_path / "rows.json"
    path.write_text('{"content":"abc"}\n{"content":"def"}\n')
    fam = Family(
        id="json",
        sources=(
            Source(
                "json", "local", "content",
                format="json",
                data_files=str(path),
            ),
        ),
    )
    calls: list[tuple[str, str | None]] = []

    monkeypatch.setattr(
        Trainer,
        "_maybe_trim_memory",
        lambda self, stage, shard=None: calls.append((stage, shard)),
    )

    run_trainer(tmp_path, [fam], workers=1)

    assert ("shard", "json/local#0") in calls


def test_remote_stream_limit_defaults_and_env(tmp_path: Path, monkeypatch):
    trainer = make_trainer(tmp_path, [], workers=16)
    assert trainer.remote_streams == 4
    trainer.events.close()

    monkeypatch.setenv("SNG_HF_STREAMS", "3")
    limited = make_trainer(tmp_path, [], workers=16)
    assert limited.remote_streams == 3
    limited.events.close()


def test_revision_pinning_rewrites_hf_globs(tmp_path: Path, monkeypatch):
    fam = local_family(tmp_path, "alpha", ["x"], files=1)
    trainer = make_trainer(tmp_path, [fam])
    trainer.state.revisions["example/github-code"] = "abc123"

    src = Source(
        "github-code",
        "example/github-code",
        "content",
        data_files="hf://datasets/example/github-code/data/*.parquet",
    )
    captured = {}

    def fake_load_dataset(path, **kw):
        captured.update(kw, path=path)
        raise RuntimeError("stop here")

    import datasets

    monkeypatch.setattr(datasets, "load_dataset", fake_load_dataset)
    with pytest.raises(RuntimeError, match="stop here"):
        trainer._load_source(src)
    assert captured["data_files"] == (
        "hf://datasets/example/github-code@abc123/data/*.parquet"
    )
    # local fixtures never pin
    assert trainer._source_revision(fam.sources[0]) is None
    trainer.events.close()
