import json
from dataclasses import replace
from pathlib import Path

import pytest

from sngram_train.catalog import Catalog, FormatSpec
from sngram_train.manifest import Candidate, ManifestBuilder, open_manifest
from sngram_train.pipeline import Trainer, TrainerConfig


class MemoryContent:
    def __init__(self, values):
        self.values = values

    def read(self, blob_id, _max_bytes):
        return self.values[blob_id]


class InterruptingContent(MemoryContent):
    def __init__(self, values, interrupt_at):
        super().__init__(values)
        self.interrupt_at = interrupt_at
        self.calls = 0

    def read(self, blob_id, max_bytes):
        self.calls += 1
        if self.calls == self.interrupt_at:
            raise KeyboardInterrupt
        return super().read(blob_id, max_bytes)


def setup_run(tmp_path: Path, lengths: dict[str, list[int]], target: int):
    formats = tuple(
        FormatSpec(format_id, "code", format_id, target)
        for format_id in sorted(lengths)
    )
    catalog = Catalog(formats, tuple(sorted(lengths)))
    roster_hash = catalog.roster_hash("revision", target)
    manifest_path = tmp_path / "manifest.sqlite3"
    content = {}
    with ManifestBuilder(manifest_path, "revision", roster_hash) as builder:
        for spec in formats:
            builder.register(spec.id)
            for index, length in enumerate(lengths[spec.id]):
                blob_id = f"{spec.id}-{index}"
                content[blob_id] = bytes([65 + index]) * length
                builder.add(Candidate(spec.id, blob_id, "utf-8", length, 1))
    manifest = open_manifest(manifest_path, roster_hash)
    config = TrainerConfig(
        mint_dir=tmp_path / "bins",
        target=target,
        mint_cadence=target // 2,
        workers=4,
        checkpoint_interval=3600,
        resume=False,
    )
    return Trainer(catalog, manifest, MemoryContent(content), config, {"code": 1})


def mint_events(tmp_path: Path):
    path = tmp_path / "bins" / "train-events.jsonl"
    return [
        json.loads(line)
        for line in path.read_text().splitlines()
        if json.loads(line)["kind"] == "mint"
    ]


def test_every_mint_has_exact_area_and_format_balance(tmp_path: Path):
    trainer = setup_run(tmp_path, {"a": [20] * 4, "b": [20] * 4}, target=120)

    trainer.run()

    events = mint_events(tmp_path)
    assert [event["effective_bytes"] for event in events] == [60, 120]
    assert events[0]["formats"] == {"a": 30, "b": 30}
    assert events[1]["formats"] == {"a": 60, "b": 60}
    assert trainer.counter.bytes_processed == 120


def test_exhausted_format_quota_moves_inside_its_area(tmp_path: Path):
    trainer = setup_run(tmp_path, {"a": [20], "b": [20] * 6}, target=100)

    trainer.run()

    final = mint_events(tmp_path)[-1]
    assert final["areas"] == {"code": 100}
    assert final["formats"] == {"a": 20, "b": 80}


def test_interrupted_fetch_round_resumes_to_identical_table(tmp_path: Path):
    lengths = {"a": [20] * 4, "b": [20] * 4}
    interrupted = setup_run(tmp_path / "run", lengths, target=120)
    values = interrupted.content.values
    interrupted.content = InterruptingContent(values, interrupt_at=3)

    with pytest.raises(KeyboardInterrupt):
        interrupted.run()

    resumed = Trainer(
        interrupted.catalog,
        interrupted.manifest,
        MemoryContent(values),
        replace(interrupted.config, resume=True),
        {"code": 1},
    )
    resumed.run()
    reference = setup_run(tmp_path / "reference", lengths, target=120)
    reference.run()

    resumed_table = (tmp_path / "run" / "bins" / "final_weights.bin").read_bytes()
    reference_table = (tmp_path / "reference" / "bins" / "final_weights.bin").read_bytes()
    assert resumed_table == reference_table
