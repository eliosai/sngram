"""Row-group-level streaming: a shard (one parquet file) is counted one row
group at a time so a transient mid-file failure re-reads only the in-progress
row group, never the whole multi-GB file, and an actively-downloading worker is
never mistaken for a stall. These pin the fix for the symptom where giant
shards went yellow→red in the dashboard and a connection drop discarded
gigabytes of already-counted work."""

from __future__ import annotations

from collections import Counter
from pathlib import Path

import gzip

import pyarrow as pa
import pyarrow.parquet as pq

from sngram_train.config import Family, Source
from sngram_train.pipeline import Trainer, WorkerState, _HeartbeatFile, _attach_read_heartbeat
from tests.test_pipeline import run_trainer


def _rg_family(tmp_path: Path, rows: list[str], row_group_size: int) -> Family:
    directory = tmp_path / "rg"
    directory.mkdir(parents=True, exist_ok=True)
    tbl = pa.table({"content": pa.array(rows, type=pa.large_string())})
    pq.write_table(tbl, directory / "rg-0.parquet", row_group_size=row_group_size)
    glob = str(directory / "rg-*.parquet")
    return Family(id="alpha", sources=(Source("alpha", "local", "content", data_files=glob),))


def test_transient_failure_midfile_resumes_without_rereading(tmp_path: Path, monkeypatch):
    # 100 rows across 10 row groups; a transient connection reset strikes at the
    # START of row group 2 (after 0 and 1 have committed). The retry must resume
    # at row group 2 — NOT re-read 0 and 1 — and the file must count exactly once.
    rows = ["hello world! " * 4] * 100
    fam = _rg_family(tmp_path, rows, row_group_size=10)

    decoded: list[tuple[int | None, int]] = []  # (row_group, rows) per decoded batch
    requested: list[int | None] = []            # row groups handed to iter_batches
    fired = {"x": False}
    real_open = Trainer._open_parquet

    def patched_open(self, url, ws=None):
        pf, fh = real_open(self, url, ws)

        class Wrap:
            schema_arrow = pf.schema_arrow
            num_row_groups = pf.num_row_groups

            def iter_batches(self_w, *a, row_groups=None, **k):
                rg = None if row_groups is None else row_groups[0]
                requested.append(rg)
                if rg == 2 and not fired["x"]:
                    fired["x"] = True
                    raise ConnectionError("connection reset by peer")
                for b in pf.iter_batches(*a, row_groups=row_groups, **k):
                    decoded.append((rg, b.num_rows))
                    yield b

        return Wrap(), fh

    monkeypatch.setattr(Trainer, "_open_parquet", patched_open)
    trainer = run_trainer(tmp_path, [fam], workers=1)

    expected = sum(len(r.encode()) for r in rows)
    assert trainer.durable_bytes() == expected           # merged exactly once
    assert trainer.failed_shards == 0                    # transient, not terminal
    assert trainer.counter.files_processed == 1          # one file == one shard

    per_rg = Counter()
    for rg, n in decoded:
        per_rg[rg] += n
    assert sum(per_rg.values()) == 100, f"re-read: {sum(per_rg.values())} rows decoded, want 100"
    assert per_rg[0] == 10 and per_rg[1] == 10           # committed pre-fault, not re-read
    assert requested.count(2) == 2                       # rg2: failed attempt + retry


def test_in_flight_bytes_settle_to_zero_after_midfile_retry(tmp_path: Path, monkeypatch):
    # the in-progress row group's bytes are dropped on failure while committed
    # row groups stay in flight; once the file merges, in_flight nets to zero and
    # total == durable (no leaked or double-counted in-flight bytes)
    rows = ["abcdefghijklmno"] * 80
    fam = _rg_family(tmp_path, rows, row_group_size=8)  # 10 row groups
    fired = {"x": False}
    real_open = Trainer._open_parquet

    def patched_open(self, url, ws=None):
        pf, fh = real_open(self, url, ws)

        class Wrap:
            schema_arrow = pf.schema_arrow
            num_row_groups = pf.num_row_groups

            def iter_batches(self_w, *a, row_groups=None, **k):
                rg = None if row_groups is None else row_groups[0]
                if rg == 5 and not fired["x"]:
                    fired["x"] = True
                    # fail PART-WAY through the row group, after some bytes moved
                    for i, b in enumerate(pf.iter_batches(*a, row_groups=row_groups, **k)):
                        if i == 0:
                            yield b
                        raise TimeoutError("read timed out")
                yield from pf.iter_batches(*a, row_groups=row_groups, **k)

        return Wrap(), fh

    monkeypatch.setattr(Trainer, "_open_parquet", patched_open)
    trainer = run_trainer(tmp_path, [fam], workers=1)

    expected = sum(len(r.encode()) for r in rows)
    assert trainer.durable_bytes() == expected
    assert trainer.in_flight_bytes == 0           # nothing leaked from the dropped row group
    assert trainer.total_bytes() == expected      # total == durable, no double count


def test_retry_log_keeps_incomplete_body_numbers(tmp_path: Path, monkeypatch):
    monkeypatch.setattr("sngram_train.pipeline.RETRY_BASE_S", 0.01)
    monkeypatch.setattr("sngram_train.pipeline.RETRY_CAP_S", 0.02)
    rows = ["hello world"] * 12
    fam = _rg_family(tmp_path, rows, row_group_size=6)
    fired = {"x": False}
    real_open = Trainer._open_parquet

    def patched_open(self, url, ws=None):
        pf, fh = real_open(self, url, ws)

        class Wrap:
            schema_arrow = pf.schema_arrow
            num_row_groups = pf.num_row_groups

            def iter_batches(self_w, *a, row_groups=None, **k):
                if not fired["x"]:
                    fired["x"] = True
                    raise Exception(
                        "RemoteProtocolError: peer closed connection without sending "
                        "complete message body (received 404195355 bytes, "
                        "expected 511313345)"
                    )
                yield from pf.iter_batches(*a, row_groups=row_groups, **k)

        return Wrap(), fh

    monkeypatch.setattr(Trainer, "_open_parquet", patched_open)
    trainer = run_trainer(tmp_path, [fam], workers=1)

    warn = next(e for e in trainer.events.tail if e.get("stage") == "shard")
    assert warn["error_kind"] == "transient"
    assert warn["error_type"] == "remoteprotocolerror"
    assert warn["received_bytes"] == 404195355
    assert warn["expected_bytes"] == 511313345


def test_read_heartbeat_bumps_progress_during_fetch():
    # a worker downloading a large row group (no batch decoded yet) must still
    # look alive to the watchdog: every underlying read advances last_progress
    ws = WorkerState()
    ws.last_progress = 0.0

    class FakeFile:
        def read(self, n=-1):
            return b"x" * 8

    fh = FakeFile()
    _attach_read_heartbeat(fh, ws)
    assert fh.read(8) == b"xxxxxxxx"
    assert ws.last_progress > 0.0


def test_read_heartbeat_degrades_on_immutable_handle():
    # a C-level handle that forbids reassigning `read` must not crash the worker;
    # it just falls back to per-batch heartbeats
    ws = WorkerState()

    class Immutable:
        __slots__ = ()

        def read(self, n=-1):
            return b""

    _attach_read_heartbeat(Immutable(), ws)  # no exception


def test_gzip_raw_stream_heartbeat_tracks_underlying_reads():
    ws = WorkerState()
    ws.last_progress = 0.0
    payload = gzip.compress(b'{"content":"abc"}\n' * 10)

    class SlowBytes:
        def __init__(self, data: bytes) -> None:
            self.data = bytearray(data)

        def read(self, n=-1):
            if n is None or n < 0:
                n = len(self.data)
            out = bytes(self.data[:n])
            del self.data[:n]
            return out

    gz = gzip.GzipFile(fileobj=_HeartbeatFile(SlowBytes(payload), ws))
    assert gz.readline()
    assert ws.last_progress > 0.0


def test_multi_rowgroup_file_is_one_shard(tmp_path: Path):
    # a file split into many row groups counts every byte exactly once and is
    # accounted as a single shard / single family completion, not one per group
    rows = ["abcdefghij"] * 50
    fam = _rg_family(tmp_path, rows, row_group_size=5)  # 10 row groups
    trainer = run_trainer(tmp_path, [fam], workers=1)

    assert trainer.durable_bytes() == 50 * 10
    assert trainer.counter.files_processed == 1
    assert trainer.state.family_done == {"alpha": 1}
