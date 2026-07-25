from types import SimpleNamespace

import pytest

from sngram_train import corpus
from sngram_train.errors import ConfigurationError


class FakeApi:
    info = None

    def __init__(self, token=None):
        pass

    def dataset_info(self, repo, files_metadata=False):
        return FakeApi.info


def info_with(files):
    return SimpleNamespace(
        sha="rev-abc",
        siblings=[SimpleNamespace(rfilename=name, size=size) for name, size in files],
    )


def test_resolve_corpus_lists_sorted_parquet_shards(monkeypatch):
    FakeApi.info = info_with(
        [
            ("data/part-00001.parquet", 20),
            ("README.md", 5),
            ("data/part-00000.parquet", 10),
            ("data/stats.json", 3),
        ]
    )
    monkeypatch.setattr("huggingface_hub.HfApi", FakeApi)

    resolved = corpus.resolve_corpus(token=None)

    assert resolved.revision == "rev-abc"
    assert [shard.name for shard in resolved.shards] == [
        "data/part-00000.parquet",
        "data/part-00001.parquet",
    ]
    assert resolved.wire_bytes() == 30
    assert resolved.take(1).wire_bytes() == 10


def test_resolve_corpus_without_shards_fails_with_guidance(monkeypatch):
    FakeApi.info = info_with([("README.md", 5)])
    monkeypatch.setattr("huggingface_hub.HfApi", FakeApi)

    with pytest.raises(ConfigurationError, match="parquet shards"):
        corpus.resolve_corpus(token=None)


def test_corpus_repo_honours_the_environment(monkeypatch):
    monkeypatch.setenv("SNGRAM_DATASET_REPO", "other/repo")
    assert corpus.corpus_repo() == "other/repo"
    monkeypatch.delenv("SNGRAM_DATASET_REPO")
    assert corpus.corpus_repo() == corpus.DEFAULT_REPO
