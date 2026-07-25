from types import SimpleNamespace

import pytest

from sngram_train import corpus
from sngram_train.errors import ConfigurationError


class FakeApi:
    info = None

    def __init__(self, token=None):
        pass

    def dataset_info(self, repo, files_metadata=False, timeout=None):
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


class FlakyApi:
    """Fails the first `fail_times` listings, then succeeds"""

    fail_times = 0
    error: Exception = RuntimeError("boom")
    calls: list[float | None] = []

    def __init__(self, token=None):
        pass

    def dataset_info(self, repo, files_metadata=False, timeout=None):
        FlakyApi.calls.append(timeout)
        if len(FlakyApi.calls) <= FlakyApi.fail_times:
            raise FlakyApi.error
        return FakeApi.info


class ReadTimeoutError(Exception):
    """Named to match the transient classifier"""


def _arm(monkeypatch, error: Exception, fail_times: int):
    FakeApi.info = info_with([("data/part-00000.parquet", 11)])
    FlakyApi.calls = []
    FlakyApi.error = error
    FlakyApi.fail_times = fail_times
    monkeypatch.setattr("huggingface_hub.HfApi", FlakyApi)
    monkeypatch.setattr(corpus, "_LISTING_BACKOFF", 0.0)


def test_listing_bounds_each_attempt_with_a_timeout(monkeypatch):
    _arm(monkeypatch, ReadTimeoutError("stall"), 0)

    corpus.resolve_corpus(None)

    assert FlakyApi.calls == [corpus._LISTING_TIMEOUT], (
        "a listing with no timeout hangs forever on a half-open socket"
    )


def test_transient_listing_failure_is_retried(monkeypatch):
    _arm(monkeypatch, ReadTimeoutError("stall"), 2)
    notes: list[str] = []

    resolved = corpus.resolve_corpus(None, notes.append)

    assert len(resolved.shards) == 1
    assert len(FlakyApi.calls) == 3
    assert any("retrying" in note for note in notes)


def test_unexpected_listing_failure_is_not_retried(monkeypatch):
    _arm(monkeypatch, ValueError("bad credentials"), 5)

    with pytest.raises(ValueError, match="bad credentials"):
        corpus.resolve_corpus(None)

    assert len(FlakyApi.calls) == 1, "only transport failures deserve a retry"
