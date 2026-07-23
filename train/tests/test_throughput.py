import time
from collections import Counter
from pathlib import Path

from sngram_train.catalog import Catalog, FormatSpec
from sngram_train.manifest import Candidate, ManifestBuilder, open_manifest
from sngram_train.pipeline import Trainer, TrainerConfig

LINE = b"fn main() { return 42; }\n"


def build_trainer(tmp_path: Path, lengths, target, content, cadence=None, workers=16):
    formats = tuple(
        FormatSpec(name, "code", name, target) for name in sorted(lengths)
    )
    catalog = Catalog(formats, tuple(sorted(lengths)))
    roster_hash = catalog.roster_hash("revision")
    path = tmp_path / "manifest.sqlite3"
    with ManifestBuilder(path, "revision", roster_hash) as builder:
        for spec in formats:
            builder.register(spec.id)
            for index, length in enumerate(lengths[spec.id]):
                builder.add(Candidate(spec.id, f"{spec.id}-{index}", "utf-8", length, 1))
            builder.set_exhausted(spec.id)
    manifest = open_manifest(path, roster_hash)
    config = TrainerConfig(
        mint_dir=tmp_path / "bins",
        target=target,
        mint_cadence=cadence or target,
        workers=workers,
        checkpoint_interval=3600,
        resume=False,
    )
    return Trainer(catalog, manifest, content, config, {"code": 1})


class LatencyContent:
    def __init__(self, delays):
        self.delays = delays
        self.reads = Counter()

    def read(self, blob_id, max_bytes):
        self.reads[blob_id] += 1
        format_id, _, _index = blob_id.rpartition("-")
        time.sleep(self.delays.get(format_id, 0.0))
        return LINE * (max_bytes // len(LINE))


def test_slow_format_does_not_stall_the_other_formats(tmp_path: Path):
    doc = len(LINE) * 80
    lengths = {"slow": [doc] * 6}
    lengths.update({f"fast{i}": [doc] * 40 for i in range(7)})
    total = doc * (6 + 7 * 40)
    delays = {"slow": 0.3}
    delays.update({f"fast{i}": 0.001 for i in range(7)})
    content = LatencyContent(delays)
    trainer = build_trainer(tmp_path, lengths, total, content)

    started = time.monotonic()
    trainer.run()
    wall = time.monotonic() - started

    assert trainer.counter.bytes_processed == total
    assert wall < 2.0


def test_uniform_latency_overlaps_fetches_across_formats(tmp_path: Path):
    doc = len(LINE) * 80
    lengths = {f"f{i}": [doc] * 25 for i in range(8)}
    total = doc * 8 * 25
    content = LatencyContent({f"f{i}": 0.02 for i in range(8)})
    trainer = build_trainer(tmp_path, lengths, total, content)

    started = time.monotonic()
    trainer.run()
    wall = time.monotonic() - started

    ideal = 8 * 25 * 0.02 / 16
    assert trainer.counter.bytes_processed == total
    assert wall < ideal * 2.5 + 0.3


def test_many_formats_do_not_throttle_the_planner(tmp_path: Path):
    doc = len(LINE) * 1000
    lengths = {f"f{i:03d}": [doc] * 24 for i in range(330)}
    total = doc * 24 * 330 // 3
    content = LatencyContent({})
    trainer = build_trainer(tmp_path, lengths, total, content, workers=64)

    started = time.monotonic()
    trainer.run()
    wall = time.monotonic() - started

    objects = sum(item.objects for item in trainer.state.formats.values())
    assert trainer.counter.bytes_processed == total
    assert objects / wall > 400


def test_partial_documents_carry_across_thresholds_without_refetch(tmp_path: Path):
    doc = len(LINE) * 280
    content = LatencyContent({})
    trainer = build_trainer(
        tmp_path, {"only": [doc] * 10}, doc * 10, content, cadence=15_000
    )

    trainer.run()

    assert trainer.counter.bytes_processed == doc * 10
    assert all(count == 1 for count in content.reads.values())
    assert len(content.reads) == 10
