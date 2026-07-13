import json
from pathlib import Path

from sngram_train.catalog import Catalog, FormatSpec
from sngram_train.manifest import open_manifest
from sngram_train.pipeline import Trainer, TrainerConfig
from sngram_train.stack import build_stack_manifest, extend_manifest


class FakeRows:
    revision = "revision-1"

    def __init__(self, rows):
        self.rows = rows

    def iter_rows(self, config, cursor=(0, 0)):
        for index, value in enumerate(self.rows[config][cursor[1] :], cursor[1]):
            item = dict(value)
            item["_source_cursor"] = (0, index + 1)
            yield item


class LossyContent:
    def __init__(self, values):
        self.values = values

    def read(self, blob_id, _max_bytes):
        if blob_id not in self.values:
            raise FileNotFoundError(blob_id)
        return self.values[blob_id]


def row(config, index, length, encoding="utf-8"):
    return {
        "blob_id": f"{config}-{index}",
        "content_id": f"{config}-{index}",
        "src_encoding": encoding,
        "language": config,
        "path": f"/{config}/{index}",
        "extension": "x",
        "is_vendor": False,
        "is_generated": False,
        "length_bytes": length,
        "_sample_weight": 1,
    }


def lossy_corpus(configs, per_config, length):
    """Rows where every third blob shrinks by half and every tenth is missing."""

    rows, content = {}, {}
    for config in configs:
        config_rows = []
        for index in range(per_config):
            blob = f"{config}-{index}"
            if index % 10 == 9:
                config_rows.append(row(config, index, length))
            elif index % 3 == 0:
                text = "half sized text pair\n" * (length // 42)
                raw = text.encode("utf-16")
                content[blob] = raw
                config_rows.append(row(config, index, len(raw), "utf-16"))
            else:
                content[blob] = b"full sized code line\n" * (length // 21)
                config_rows.append(row(config, index, length))
        rows[config] = config_rows
    return rows, content


def setup(tmp_path: Path, configs, target, rows, content, cadence=None):
    formats = tuple(FormatSpec(name, "code", name, target) for name in sorted(configs))
    catalog = Catalog(formats, tuple(sorted(configs)))
    manifest_path = tmp_path / "manifest.sqlite3"
    source = FakeRows(rows)
    roster_hash = build_stack_manifest(
        manifest_path, catalog, source, target=target, area_weights={"code": 1}
    )
    manifest = open_manifest(manifest_path, roster_hash)
    config = TrainerConfig(
        mint_dir=tmp_path / "bins",
        target=target,
        mint_cadence=cadence or max(target // 3, 1),
        workers=8,
        checkpoint_interval=3600,
        resume=False,
    )

    def extend(format_id, minimum):
        extend_manifest(manifest_path, catalog, source, roster_hash, format_id, minimum)
        return open_manifest(manifest_path, roster_hash)

    trainer = Trainer(
        catalog, manifest, LossyContent(content), config, {"code": 1}, extend=extend
    )
    return trainer


def events_of(tmp_path: Path, kind: str):
    path = tmp_path / "bins" / "train-events.jsonl"
    return [
        event
        for event in map(json.loads, path.read_text().splitlines())
        if event["kind"] == kind
    ]


def test_lossy_delivery_completes_exactly_through_manifest_extension(tmp_path: Path):
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
    assert events_of(tmp_path, "manifest_extend")


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


def test_inventory_clamp_flows_into_a_shorter_exact_schedule(tmp_path: Path):
    configs = ["a", "b"]
    rows = {name: [row(name, i, 2_100) for i in range(20)] for name in configs}
    content = {
        f"{name}-{i}": b"full sized code line\n" * 100
        for name in configs
        for i in range(20)
    }
    trainer = setup(
        tmp_path, configs, target=10_000_000, rows=rows, content=content, cadence=10_000
    )

    assert trainer.effective_target == trainer.manifest.effective_target
    assert trainer.effective_target < 10_000_000

    trainer.run()

    assert trainer.counter.bytes_processed == trainer.effective_target
    assert not trainer.clamped
    labels = [event["label"] for event in events_of(tmp_path, "mint")]
    assert labels
    assert (tmp_path / "bins" / "final_weights.bin").exists()


def test_lossy_run_resumes_after_interrupt_to_the_identical_table(tmp_path: Path):
    configs = ["a", "b", "c"]
    rows, content = lossy_corpus(configs, per_config=200, length=2_100)

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

    interrupted = setup(tmp_path / "run", configs, 300_000, rows, content)
    interrupted.content = InterruptingContent(content, interrupt_at=150)
    try:
        interrupted.run()
    except KeyboardInterrupt:
        pass

    resumed = Trainer(
        interrupted.catalog,
        interrupted.manifest,
        LossyContent(content),
        TrainerConfig(
            mint_dir=tmp_path / "run" / "bins",
            target=300_000,
            mint_cadence=100_000,
            workers=8,
            checkpoint_interval=3600,
            resume=True,
        ),
        {"code": 1},
        extend=interrupted.extend,
    )
    resumed.run()
    reference = setup(tmp_path / "reference", configs, 300_000, rows, content)
    reference.run()

    resumed_table = (tmp_path / "run" / "bins" / "final_weights.bin").read_bytes()
    reference_table = (tmp_path / "reference" / "bins" / "final_weights.bin").read_bytes()
    assert resumed_table == reference_table
