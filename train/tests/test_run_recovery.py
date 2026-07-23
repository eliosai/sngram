import json
from pathlib import Path

from sngram_train.catalog import Catalog, FormatSpec
from sngram_train.manifest import ManifestWriter, open_manifest
from sngram_train.pipeline import Trainer, TrainerConfig


class LossyContent:
    def __init__(self, values):
        self.values = values

    def read(self, blob_id, _max_bytes):
        if blob_id not in self.values:
            raise FileNotFoundError(blob_id)
        return self.values[blob_id]


class InterruptingContent(LossyContent):
    def __init__(self, values, interrupt_at):
        super().__init__(values)
        self.interrupt_at = interrupt_at
        self.calls = 0

    def read(self, blob_id, max_bytes):
        self.calls += 1
        if self.calls == self.interrupt_at:
            raise KeyboardInterrupt
        return super().read(blob_id, max_bytes)


def lossy_corpus(configs, per_config, length):
    """Rows where every third blob shrinks by half and every tenth is missing."""

    rows, content = {}, {}
    for config in configs:
        config_rows = []
        for index in range(per_config):
            blob = f"{config}-{index}"
            if index % 10 == 9:
                config_rows.append((blob, "utf-8", length, 1, "", ""))
            elif index % 3 == 0:
                text = "half sized text pair\n" * (length // 42)
                raw = text.encode("utf-16")
                content[blob] = raw
                config_rows.append((blob, "utf-16", len(raw), 1, "", ""))
            else:
                content[blob] = b"full sized code line\n" * (length // 21)
                config_rows.append((blob, "utf-8", length, 1, "", ""))
        rows[config] = config_rows
    return rows, content


def setup(tmp_path: Path, configs, target, rows, content, effective_target=None):
    formats = tuple(FormatSpec(name, "code", name, target) for name in sorted(configs))
    catalog = Catalog(formats, tuple(sorted(configs)))
    roster_hash = catalog.roster_hash("revision")
    manifest_path = tmp_path / "manifest.sqlite3"
    with ManifestWriter(manifest_path, "revision", roster_hash) as writer:
        for name in sorted(configs):
            writer.register(name)
            writer.add_rows(name, rows[name])
        writer.set_targets(None, effective_target)
    manifest = open_manifest(manifest_path, roster_hash)
    config = TrainerConfig(
        mint_dir=tmp_path / "bins",
        target=target,
        workers=8,
        checkpoint_interval=3600,
        resume=False,
    )
    return Trainer(catalog, manifest, LossyContent(content), config, {"code": 1})


def events_of(tmp_path: Path, kind: str):
    path = tmp_path / "bins" / "train-events.jsonl"
    return [
        event
        for event in map(json.loads, path.read_text().splitlines())
        if event["kind"] == kind
    ]


def test_lossy_delivery_completes_exactly_from_manifest_headroom(tmp_path: Path):
    configs = ["a", "b", "c", "d"]
    rows, content = lossy_corpus(configs, per_config=400, length=2_100)
    trainer = setup(tmp_path, configs, target=800_000, rows=rows, content=content)

    trainer.run()

    assert trainer.counter.bytes_processed == 800_000
    assert not trainer.clamped
    assert sum(trainer.format_bytes().values()) == 800_000
    final = events_of(tmp_path, "mint")[-1]
    assert final["effective_bytes"] == 800_000
    assert (tmp_path / "bins" / "final_weights.bin").exists()
    assert events_of(tmp_path, "content_skips")


def test_depleted_corpus_clamps_loudly_and_still_mints_final(tmp_path: Path):
    configs = ["a", "b"]
    rows, content = lossy_corpus(configs, per_config=40, length=2_100)
    trainer = setup(tmp_path, configs, target=50_000_000, rows=rows, content=content)

    trainer.run()

    assert trainer.clamped
    assert trainer.counter.bytes_processed > 0
    assert (tmp_path / "bins" / "final_weights.bin").exists()
    clamps = events_of(tmp_path, "target_clamped")
    assert clamps and clamps[0]["achievable"] < clamps[0]["requested"]
    summary = events_of(tmp_path, "summary")[-1]
    assert summary["complete"] is True and summary["clamped"] is True


def test_manifest_effective_target_bounds_the_run_upfront(tmp_path: Path):
    configs = ["a", "b"]
    rows = {
        name: [(f"{name}-{i}", "utf-8", 2_100, 1, "", "") for i in range(20)]
        for name in configs
    }
    content = {
        f"{name}-{i}": b"full sized code line\n" * 100
        for name in configs
        for i in range(20)
    }
    trainer = setup(
        tmp_path, configs, target=10_000_000, rows=rows, content=content,
        effective_target=60_000,
    )

    assert trainer.effective_target == 60_000

    trainer.run()

    assert trainer.counter.bytes_processed == 60_000
    assert not trainer.clamped
    assert (tmp_path / "bins" / "final_weights.bin").exists()


def test_lossy_run_resumes_after_interrupt_to_the_identical_table(tmp_path: Path):
    configs = ["a", "b", "c"]
    rows, content = lossy_corpus(configs, per_config=200, length=2_100)
    interrupted = setup(tmp_path / "run", configs, 300_000, rows, content)
    interrupted.content = InterruptingContent(content, interrupt_at=150)
    try:
        interrupted.run()
    except KeyboardInterrupt:
        pass

    resumed = _resumed_trainer(interrupted, content, tmp_path / "run" / "bins")
    resumed.run()
    reference = setup(tmp_path / "reference", configs, 300_000, rows, content)
    reference.run()

    resumed_table = (tmp_path / "run" / "bins" / "final_weights.bin").read_bytes()
    reference_table = (tmp_path / "reference" / "bins" / "final_weights.bin").read_bytes()
    assert resumed_table == reference_table


def _resumed_trainer(interrupted, content, mint_dir: Path):
    config = TrainerConfig(
        mint_dir=mint_dir,
        target=300_000,
        workers=8,
        checkpoint_interval=3600,
        resume=True,
    )
    return Trainer(
        interrupted.catalog,
        interrupted.manifest,
        LossyContent(content),
        config,
        {"code": 1},
    )
