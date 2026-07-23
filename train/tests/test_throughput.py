import time
from collections import Counter
from pathlib import Path

from sngram_train.pipeline import Trainer, TrainerConfig
from sngram_train.stream import CorpusMeta, CorpusRow
from tests.test_pipeline import ListStream

LINE = b"fn main() { return 42; }\n"


class LatencyContent:
    def __init__(self, delay, slow=frozenset(), slow_delay=0.0):
        self.delay = delay
        self.slow = slow
        self.slow_delay = slow_delay
        self.reads = Counter()

    def read(self, blob_id, max_bytes):
        self.reads[blob_id] += 1
        time.sleep(self.slow_delay if blob_id in self.slow else self.delay)
        return LINE * (max_bytes // len(LINE))


def build(tmp_path: Path, count, doc, content, workers=16):
    rows = [CorpusRow("code", f"code-{i}", "utf-8", doc, 1) for i in range(count)]
    meta = CorpusMeta(
        "revision", "corpus-1", count, count * doc, count * doc, {"code": count * doc}
    )
    factory = lambda state: ListStream(rows, (state or {}).get("position", 0))
    config = TrainerConfig(
        mint_dir=tmp_path / "bins",
        workers=workers,
        checkpoint_interval=3600,
        resume=False,
    )
    return Trainer(factory, content, config, meta)


def test_uniform_latency_overlaps_fetches(tmp_path: Path):
    doc = len(LINE) * 80
    content = LatencyContent(0.02)
    trainer = build(tmp_path, 200, doc, content)

    started = time.monotonic()
    trainer.run()
    wall = time.monotonic() - started

    ideal = 200 * 0.02 / 16
    assert trainer.counter.bytes_processed == 200 * doc
    assert wall < ideal * 2.5 + 0.3


def test_one_slow_object_does_not_stall_the_stream(tmp_path: Path):
    doc = len(LINE) * 80
    content = LatencyContent(0.001, slow={"code-5"}, slow_delay=0.3)
    trainer = build(tmp_path, 200, doc, content)

    started = time.monotonic()
    trainer.run()
    wall = time.monotonic() - started

    assert trainer.counter.bytes_processed == 200 * doc
    assert wall < 1.5


def test_the_coordinator_keeps_pace_with_many_small_objects(tmp_path: Path):
    doc = len(LINE) * 40
    content = LatencyContent(0.0)
    trainer = build(tmp_path, 8_000, doc, content, workers=64)

    started = time.monotonic()
    trainer.run()
    wall = time.monotonic() - started

    assert trainer.counter.bytes_processed == 8_000 * doc
    assert 8_000 / wall > 1_000
