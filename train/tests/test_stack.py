from types import SimpleNamespace

import pytest

from sngram_train import stack
from sngram_train.errors import ConfigurationError


class FakeApi:
    info = None

    def __init__(self, token=None):
        pass

    def dataset_info(self, repo, files_metadata=False, timeout=None):
        return FakeApi.info


def arm(monkeypatch, files):
    FakeApi.info = SimpleNamespace(
        sha="rev-abc",
        siblings=[SimpleNamespace(rfilename=name, size=size) for name, size in files],
    )
    monkeypatch.setattr("huggingface_hub.HfApi", FakeApi)


def test_resolve_lists_sorted_parquet_shards(monkeypatch):
    arm(
        monkeypatch,
        [
            ("data/part-00001.parquet", 20),
            ("README.md", 5),
            ("data/part-00000.parquet", 10),
            ("data/stats.json", 3),
        ],
    )

    corpus = stack.resolve(token=None)

    assert corpus.identity.name == "stack-v3"
    assert corpus.identity.stamp() == "stack-v3@rev-abc"
    assert corpus.quota is None, "the single-dataset path has no byte ceilings"
    assert [shard.path for shard in corpus.shards] == [
        f"datasets/{stack.DEFAULT_REPO}@rev-abc/data/part-00000.parquet",
        f"datasets/{stack.DEFAULT_REPO}@rev-abc/data/part-00001.parquet",
    ]
    assert corpus.wire_bytes() == 30
    assert corpus.take(1).wire_bytes() == 10


def test_every_shard_reads_the_nested_files_content(monkeypatch):
    arm(monkeypatch, [("data/part-00000.parquet", 10)])

    corpus = stack.resolve(token=None)

    assert corpus.shards[0].reading == stack.READING
    assert corpus.shards[0].reading.layout == "stack-files"


def test_resolve_without_shards_fails_with_guidance(monkeypatch):
    arm(monkeypatch, [("README.md", 5)])

    with pytest.raises(ConfigurationError, match="parquet shards"):
        stack.resolve(token=None)


def test_corpus_repo_honours_the_environment(monkeypatch):
    monkeypatch.setenv("SNGRAM_DATASET_REPO", "other/repo")
    assert stack.corpus_repo() == "other/repo"
    monkeypatch.delenv("SNGRAM_DATASET_REPO")
    assert stack.corpus_repo() == stack.DEFAULT_REPO
