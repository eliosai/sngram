from types import SimpleNamespace

import pytest

from sngram_train import hub
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


def test_listing_pins_the_revision_and_sizes_every_file(monkeypatch):
    FakeApi.info = info_with([("data/part-1.parquet", 20), ("README.md", 5)])
    monkeypatch.setattr("huggingface_hub.HfApi", FakeApi)

    listed = hub.listing("org/set", token=None)

    assert listed.revision == "rev-abc"
    assert listed.files == (("README.md", 5), ("data/part-1.parquet", 20))
    assert listed.hub_path("data/part-1.parquet") == (
        "datasets/org/set@rev-abc/data/part-1.parquet"
    )


def test_under_selects_by_prefix_and_suffix_in_order(monkeypatch):
    FakeApi.info = info_with(
        [
            ("data/b.parquet", 2),
            ("data/a.parquet", 1),
            ("data/notes.json", 9),
            ("other/c.parquet", 3),
        ]
    )
    monkeypatch.setattr("huggingface_hub.HfApi", FakeApi)

    listed = hub.listing("org/set", token=None)

    assert listed.under("data/", ".parquet") == [
        ("data/a.parquet", 1),
        ("data/b.parquet", 2),
    ]


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


class GatedApi:
    def __init__(self, token=None):
        pass

    def dataset_info(self, repo, files_metadata=False, timeout=None):
        import httpx
        from huggingface_hub.errors import GatedRepoError

        request = httpx.Request("GET", f"https://huggingface.co/datasets/{repo}")
        raise GatedRepoError("no access", response=httpx.Response(403, request=request))


def _arm(monkeypatch, error: Exception, fail_times: int):
    FakeApi.info = info_with([("data/part-00000.parquet", 11)])
    FlakyApi.calls = []
    FlakyApi.error = error
    FlakyApi.fail_times = fail_times
    monkeypatch.setattr("huggingface_hub.HfApi", FlakyApi)
    monkeypatch.setattr(hub, "_LISTING_BACKOFF", 0.0)


def test_listing_bounds_each_attempt_with_a_timeout(monkeypatch):
    _arm(monkeypatch, ReadTimeoutError("stall"), 0)

    hub.listing("org/set", None)

    assert FlakyApi.calls == [hub._LISTING_TIMEOUT], (
        "a listing with no timeout hangs forever on a half-open socket"
    )


def test_transient_listing_failure_is_retried(monkeypatch):
    _arm(monkeypatch, ReadTimeoutError("stall"), 2)
    notes: list[str] = []

    listed = hub.listing("org/set", None, notes.append)

    assert len(listed.files) == 1
    assert len(FlakyApi.calls) == 3
    assert any("retrying" in note for note in notes)


def test_unexpected_listing_failure_is_not_retried(monkeypatch):
    _arm(monkeypatch, ValueError("bad credentials"), 5)

    with pytest.raises(ValueError, match="bad credentials"):
        hub.listing("org/set", None)

    assert len(FlakyApi.calls) == 1, "only transport failures deserve a retry"


def test_a_gated_repo_fails_with_guidance(monkeypatch):
    monkeypatch.setattr("huggingface_hub.HfApi", GatedApi)

    with pytest.raises(ConfigurationError, match="cannot read dataset org/set"):
        hub.listing("org/set", None)
