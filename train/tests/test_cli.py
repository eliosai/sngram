import os
from types import SimpleNamespace

import pytest
from typer.testing import CliRunner

from sngram_train import cli
from sngram_train.corpus import CorpusName
from sngram_train.errors import ConfigurationError


def capture_build(monkeypatch) -> dict:
    captured: dict = {}

    class FakeTrainer:
        def run(self):
            captured["ran"] = True

        progress = SimpleNamespace(describe=lambda: "complete")

    def fake_build(**kwargs):
        captured.update(kwargs)
        return FakeTrainer()

    monkeypatch.setattr(cli, "_production_trainer", fake_build)
    return captured


def test_train_defaults_to_the_whole_blend(monkeypatch, tmp_path):
    captured = capture_build(monkeypatch)

    result = CliRunner().invoke(
        cli.app,
        ["train", "--mint-dir", str(tmp_path / "bins"), "--no-dashboard"],
    )

    assert result.exit_code == 0, result.output
    assert captured["corpus"] is CorpusName.BLEND
    assert captured["limit"] is None
    assert captured["shards"] is None
    assert captured["ran"] is True
    assert "complete" in result.output


def test_stack_v3_stays_selectable(monkeypatch, tmp_path):
    captured = capture_build(monkeypatch)

    result = CliRunner().invoke(
        cli.app,
        [
            "train", "--mint-dir", str(tmp_path / "bins"),
            "--corpus", "stack-v3", "--no-dashboard",
        ],
    )

    assert result.exit_code == 0, result.output
    assert captured["corpus"] is CorpusName.STACK_V3


def test_an_unknown_corpus_is_refused(monkeypatch, tmp_path):
    capture_build(monkeypatch)

    result = CliRunner().invoke(
        cli.app,
        [
            "train", "--mint-dir", str(tmp_path / "bins"),
            "--corpus", "the-pile", "--no-dashboard",
        ],
    )

    assert result.exit_code != 0


def test_cli_keeps_table_inspection_and_validation_commands():
    result = CliRunner().invoke(cli.app, ["--help"])

    assert result.exit_code == 0
    assert "inspect" in result.output
    assert "fs-histogram" in result.output
    assert "fs-validate" in result.output


def test_train_bounds_hugging_face_request_time(monkeypatch, tmp_path):
    class FakeTrainer:
        def run(self):
            pass

        progress = SimpleNamespace(describe=lambda: "complete")

    monkeypatch.delenv("HF_HUB_DOWNLOAD_TIMEOUT", raising=False)
    monkeypatch.delenv("HF_HUB_ETAG_TIMEOUT", raising=False)
    monkeypatch.delenv("ARROW_DEFAULT_MEMORY_POOL", raising=False)
    monkeypatch.setattr(cli, "_production_trainer", lambda **_kwargs: FakeTrainer())

    result = CliRunner().invoke(
        cli.app,
        ["train", "--mint-dir", str(tmp_path / "bins"), "--no-dashboard"],
    )

    assert result.exit_code == 0, result.output
    assert os.environ["HF_HUB_DOWNLOAD_TIMEOUT"] == "30"
    assert os.environ["HF_HUB_ETAG_TIMEOUT"] == "30"
    assert os.environ["ARROW_DEFAULT_MEMORY_POOL"] == "system"


def test_startup_transport_failure_retries_but_configuration_error_does_not(monkeypatch):
    calls = 0

    class FakeTrainer:
        def run(self):
            pass

    def build(_resume):
        nonlocal calls
        calls += 1
        if calls == 1:
            raise OSError("temporary network failure")
        return FakeTrainer()

    monkeypatch.setattr("time.sleep", lambda _seconds: None)
    assert cli._run_until_done(build, resume=False, view=None).__class__ is FakeTrainer
    assert calls == 2

    def invalid(_resume):
        raise ConfigurationError("bad revision")

    with pytest.raises(ConfigurationError):
        cli._run_until_done(invalid, resume=False, view=None)


def test_unexpected_errors_fail_loudly_instead_of_retrying(monkeypatch):
    calls = 0

    def build(_resume):
        nonlocal calls
        calls += 1
        raise RuntimeError("deterministic bug")

    monkeypatch.setattr("time.sleep", lambda _seconds: None)
    with pytest.raises(RuntimeError, match="deterministic bug"):
        cli._run_until_done(build, resume=False, view=None)
    assert calls == 1
