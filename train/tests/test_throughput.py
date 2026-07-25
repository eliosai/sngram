import time
from pathlib import Path

from sngram_train.pipeline import Trainer, TrainerConfig
from tests.localcorpus import code_repos, decoded_bytes, write_corpus


class LatencyShards:
    """Injects open latency per shard name"""

    def __init__(self, inner, delay, slow=frozenset(), slow_delay=0.0):
        self.inner = inner
        self.delay = delay
        self.slow = slow
        self.slow_delay = slow_delay

    def open(self, name):
        time.sleep(self.slow_delay if name in self.slow else self.delay)
        return self.inner.open(name)


def run_trainer(tmp_path: Path, corpus, source, workers):
    config = TrainerConfig(
        mint_dir=tmp_path / "bins",
        workers=workers,
        checkpoint_interval=3600.0,
        resume=False,
    )
    trainer = Trainer(corpus, source, config)
    started = time.monotonic()
    trainer.run()
    return trainer, time.monotonic() - started


def test_parallel_workers_overlap_shard_reads(tmp_path: Path):
    shards = [code_repos(4) for _ in range(20)]
    corpus, source = write_corpus(tmp_path / "corpus", shards)

    trainer, wall = run_trainer(
        tmp_path, corpus, LatencyShards(source, 0.15), workers=10
    )

    assert trainer.counter.bytes_processed == decoded_bytes(shards)
    assert wall < 20 * 0.15 / 2


def test_one_slow_shard_does_not_stall_the_run(tmp_path: Path):
    shards = [code_repos(4) for _ in range(12)]
    corpus, source = write_corpus(tmp_path / "corpus", shards)
    slow = {corpus.shards[5].name}

    trainer, wall = run_trainer(
        tmp_path,
        corpus,
        LatencyShards(source, 0.01, slow=slow, slow_delay=0.6),
        workers=4,
    )

    assert trainer.counter.bytes_processed == decoded_bytes(shards)
    assert wall < 1.5
