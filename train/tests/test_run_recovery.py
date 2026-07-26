from pathlib import Path

from sngram_train.pipeline import Trainer, TrainerConfig
from tests.localcorpus import code_repos, decoded_bytes, write_corpus


class FlakyFile:
    """Raises one transient error at the given read call"""

    def __init__(self, inner, armed, fail_at):
        self.inner = inner
        self.armed = armed
        self.fail_at = fail_at
        self.reads = 0

    def read(self, *args):
        self.reads += 1
        if self.armed[0] and self.reads == self.fail_at:
            self.armed[0] = False
            raise ConnectionError("connection reset mid stream")
        return self.inner.read(*args)

    def __getattr__(self, name):
        return getattr(self.inner, name)

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return self.inner.__exit__(*exc)


class FlakyShards:
    """Arms the first opened file to fail one read"""

    def __init__(self, inner, fail_at):
        self.inner = inner
        self.armed = [True]
        self.fail_at = fail_at

    def open(self, name):
        handle = self.inner.open(name)
        if self.armed[0]:
            return FlakyFile(handle, self.armed, self.fail_at)
        return handle


def run_trainer(tmp_path: Path, corpus, source, resume=False):
    config = TrainerConfig(
        mint_dir=tmp_path / "bins",
        workers=2,
        checkpoint_interval=3600.0,
        resume=resume,
    )
    trainer = Trainer(corpus, source, config)
    trainer.run()
    return trainer


def test_a_transient_read_error_retries_in_place(tmp_path: Path, monkeypatch):
    monkeypatch.setattr("sngram_train.pipeline._RETRY_BASE", 0.0)
    shards = [code_repos(10), code_repos(10)]
    corpus, source = write_corpus(tmp_path / "corpus", shards)

    trainer = run_trainer(tmp_path / "run", corpus, FlakyShards(source, fail_at=2))
    run_trainer(tmp_path / "reference", corpus, source)

    assert trainer.counter.bytes_processed == decoded_bytes(shards)
    assert trainer.retries >= 1
    recovered = (tmp_path / "run" / "bins" / "final_weights.bin").read_bytes()
    expected = (tmp_path / "reference" / "bins" / "final_weights.bin").read_bytes()
    assert recovered == expected
