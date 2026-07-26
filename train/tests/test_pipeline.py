import time
from dataclasses import replace
from pathlib import Path

import pytest
import sngram

from sngram_train.corpus import CorpusIdentity
from sngram_train.errors import ConfigurationError
from sngram_train.pipeline import Trainer, TrainerConfig
from tests.localcorpus import (
    blend_corpus,
    blend_rows,
    code_repos,
    decoded_bytes,
    repo,
    write_corpus,
)


def build(tmp_path: Path, corpus, source, limit=None, resume=False, interval=3600.0):
    config = TrainerConfig(
        mint_dir=tmp_path / "bins",
        workers=3,
        checkpoint_interval=interval,
        limit=limit,
        resume=resume,
    )
    return Trainer(corpus, source, config)


class SlowShards:
    """Delays every open so a run spans supervise ticks"""

    def __init__(self, inner, delay):
        self.inner = inner
        self.delay = delay

    def open(self, name):
        time.sleep(self.delay)
        return self.inner.open(name)


class InterruptingShards:
    """Raises KeyboardInterrupt at the given open call"""

    def __init__(self, inner, interrupt_at):
        self.inner = inner
        self.interrupt_at = interrupt_at
        self.calls = 0

    def open(self, name):
        self.calls += 1
        if self.calls == self.interrupt_at:
            raise KeyboardInterrupt
        return self.inner.open(name)


def test_full_stream_counts_every_repo_and_mints_final(tmp_path: Path):
    shards = [code_repos(6), code_repos(4)]
    corpus, source = write_corpus(tmp_path / "corpus", shards)
    trainer = build(tmp_path, corpus, source)

    trainer.run()

    assert trainer.counter.bytes_processed == decoded_bytes(shards)
    assert trainer.state.decoded == decoded_bytes(shards)
    assert trainer.state.rows == 10
    assert trainer.counter.files_processed == 30
    assert trainer.state.langs == {"Rust": decoded_bytes(shards)}
    table = sngram.WeightTable.from_path(tmp_path / "bins" / "final_weights.bin")
    assert "stack-v3@rev-test" in (table.provenance or "")
    assert f"{decoded_bytes(shards)} content bytes" in table.provenance
    assert "vendor included" in table.provenance
    assert "Rust 100.0%" in table.provenance


def test_vendor_files_are_counted(tmp_path: Path):
    shards = [
        [
            repo(("clean\n", "Python", False), ("ZZZZ\n" * 8, "Go", True)),
            repo(("more clean text\n", "Python", False)),
        ]
    ]
    corpus, source = write_corpus(tmp_path / "corpus", shards)
    trainer = build(tmp_path, corpus, source)

    trainer.run()

    assert trainer.counter.count(ord("Z"), ord("Z")) > 0
    assert trainer.state.vendor_files == 1
    assert trainer.state.decoded == decoded_bytes(shards)
    assert trainer.state.langs == {"Python": 22, "Go": 40}


def test_limit_stops_the_stream_early(tmp_path: Path):
    shards = [code_repos(3) for _ in range(40)]
    corpus, source = write_corpus(tmp_path / "corpus", shards)
    trainer = build(tmp_path, corpus, SlowShards(source, 0.1), limit=500)

    trainer.run()

    assert 500 <= trainer.state.decoded < decoded_bytes(shards)
    assert (tmp_path / "bins" / "final_weights.bin").exists()


def test_interrupted_run_resumes_to_the_identical_table(tmp_path: Path):
    shards = [code_repos(12) for _ in range(4)]
    corpus, source = write_corpus(tmp_path / "corpus", shards)

    stopped = build(tmp_path / "run", corpus, InterruptingShards(source, 4))
    with pytest.raises(KeyboardInterrupt):
        stopped.run()
    assert stopped.state.decoded < decoded_bytes(shards)

    resumed = build(tmp_path / "run", corpus, source, resume=True)
    resumed.run()
    reference = build(tmp_path / "reference", corpus, source)
    reference.run()

    resumed_table = (tmp_path / "run" / "bins" / "final_weights.bin").read_bytes()
    reference_table = (tmp_path / "reference" / "bins" / "final_weights.bin").read_bytes()
    assert resumed_table == reference_table
    assert resumed.counter.bytes_processed == decoded_bytes(shards)


def test_completed_run_leaves_no_spool_files(tmp_path: Path):
    corpus, source = write_corpus(tmp_path / "corpus", [code_repos(3)])
    trainer = build(tmp_path, corpus, source)

    trainer.run()

    assert not (tmp_path / "bins" / ".spool").exists()


def test_checkpoint_rejects_a_different_corpus_revision(tmp_path: Path):
    corpus, source = write_corpus(tmp_path / "corpus", [code_repos(4)])
    build(tmp_path, corpus, source).run()

    drifted = replace(corpus, identity=CorpusIdentity("stack-v3", "other"))
    with pytest.raises(ConfigurationError, match="another revision"):
        build(tmp_path, drifted, source, resume=True)


def test_a_run_refuses_to_resume_into_a_different_corpus(tmp_path: Path):
    corpus, source = write_corpus(tmp_path / "corpus", [code_repos(4)])
    build(tmp_path, corpus, source).run()

    other = replace(corpus, identity=CorpusIdentity("blend", corpus.identity.fingerprint))
    with pytest.raises(ConfigurationError, match="corpus 'stack-v3'"):
        build(tmp_path, other, source, resume=True)


def test_no_resume_starts_a_fresh_run(tmp_path: Path):
    shards = [code_repos(4)]
    corpus, source = write_corpus(tmp_path / "corpus", shards)
    build(tmp_path, corpus, source).run()

    fresh = build(tmp_path, corpus, source, resume=False)
    fresh.run()

    assert fresh.counter.bytes_processed == decoded_bytes(shards)


def test_eta_with_a_limit_uses_the_average_decoded_rate(tmp_path: Path):
    corpus, source = write_corpus(tmp_path / "corpus", [code_repos(2)])
    trainer = build(tmp_path, corpus, source, limit=10_000)
    trainer.progress.decoded.started_at = time.monotonic() - 10.0
    trainer.state.decoded = 5_000
    trainer.progress.decoded.sample(0)
    trainer.progress.decoded.sample(5_000)

    eta = trainer.progress.eta_seconds()

    assert eta is not None
    assert 8.0 < eta < 12.0


def test_eta_without_a_limit_uses_the_average_wire_rate(tmp_path: Path):
    corpus, source = write_corpus(tmp_path / "corpus", [code_repos(2)])
    trainer = build(tmp_path, corpus, source)
    trainer.progress.wire.started_at = time.monotonic() - 10.0
    trainer.state.shard_bytes = trainer.wire_target // 2
    trainer.progress.wire.sample(0)
    trainer.progress.wire.sample(trainer.state.shard_bytes)

    eta = trainer.progress.eta_seconds()

    assert eta is not None
    assert 8.0 < eta < 12.0


def test_resumed_run_average_rate_starts_from_zero(tmp_path: Path):
    shards = [code_repos(8)]
    corpus, source = write_corpus(tmp_path / "corpus", shards)
    build(tmp_path, corpus, source).run()

    resumed = build(tmp_path, corpus, source, resume=True)

    assert resumed.state.decoded == decoded_bytes(shards)
    assert resumed.progress.rate_avg() < 1.0


BLEND_SOURCES = {"wide": blend_rows("w", 40), "narrow": blend_rows("n", 40)}


def blend_run(tmp_path: Path, caps: dict[str, int], workers: int = 1):
    corpus, opener = blend_corpus(tmp_path / "corpus", BLEND_SOURCES, caps)
    config = TrainerConfig(tmp_path / "bins", workers, 3600.0, None, False)
    trainer = Trainer(corpus, opener, config)
    trainer.run()
    return trainer


def test_a_blend_run_enforces_every_byte_ceiling(tmp_path: Path):
    trainer = blend_run(tmp_path, {"wide": 750, "narrow": 250})

    assert trainer.state.tallies.family_bytes == {"wide": 750, "narrow": 250}
    assert trainer.state.decoded == 1000
    assert trainer.counter.bytes_processed == 1000


def test_a_blend_ceiling_trims_the_boundary_batch_rather_than_overshooting(tmp_path):
    trainer = blend_run(tmp_path, {"wide": 250, "narrow": 130})

    assert trainer.state.tallies.family_bytes == {"wide": 250, "narrow": 130}
    assert trainer.state.decoded == 380, "the last shard lands in part, not whole"


def test_a_blend_run_stamps_its_corpus_and_family_mix(tmp_path: Path):
    trainer = blend_run(tmp_path, {"wide": 750, "narrow": 250})

    table = sngram.WeightTable.from_path(tmp_path / "bins" / "final_weights.bin")
    assert "blend@fingerprint-" in table.provenance
    assert "wide 75.0%" in table.provenance
    assert "narrow 25.0%" in table.provenance


def test_an_interrupted_blend_resumes_to_the_identical_table(tmp_path: Path):
    caps = {"wide": 750, "narrow": 250}
    corpus, opener = blend_corpus(tmp_path / "corpus", BLEND_SOURCES, caps)

    stopped = Trainer(
        corpus,
        InterruptingShards(opener, 4),
        TrainerConfig(tmp_path / "run", 1, 3600.0, None, False),
    )
    with pytest.raises(KeyboardInterrupt):
        stopped.run()

    resumed = Trainer(
        corpus, opener, TrainerConfig(tmp_path / "run", 1, 3600.0, None, True)
    )
    resumed.run()
    reference = Trainer(
        corpus, opener, TrainerConfig(tmp_path / "reference", 1, 3600.0, None, False)
    )
    reference.run()

    assert resumed.state.tallies.family_bytes == {"wide": 750, "narrow": 250}
    assert (tmp_path / "run" / "final_weights.bin").read_bytes() == (
        tmp_path / "reference" / "final_weights.bin"
    ).read_bytes()
