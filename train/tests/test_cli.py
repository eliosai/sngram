import pytest
from typer.testing import CliRunner

from sngram_train import cli
from sngram_train.errors import ConfigurationError


def test_train_defaults_to_canonical_ten_tb(monkeypatch, tmp_path):
    captured = {}

    class FakeTrainer:
        def run(self):
            captured["ran"] = True

        def describe_progress(self):
            return "complete"

    def fake_build(**kwargs):
        captured.update(kwargs)
        return FakeTrainer()

    monkeypatch.setattr(cli, "hf_token", lambda: "token")
    monkeypatch.setattr(cli, "_production_trainer", fake_build)

    result = CliRunner().invoke(
        cli.app,
        ["train", "--mint-dir", str(tmp_path / "bins"), "--no-dashboard"],
    )

    assert result.exit_code == 0, result.output
    assert captured["target"] == 10_000_000_000_000
    assert captured["ran"] is True
    assert "complete" in result.output


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

        def describe_progress(self):
            return "complete"

    monkeypatch.delenv("HF_HUB_DOWNLOAD_TIMEOUT", raising=False)
    monkeypatch.delenv("HF_HUB_ETAG_TIMEOUT", raising=False)
    monkeypatch.setattr(cli, "hf_token", lambda: "token")
    monkeypatch.setattr(cli, "_production_trainer", lambda **_kwargs: FakeTrainer())

    result = CliRunner().invoke(
        cli.app,
        ["train", "--mint-dir", str(tmp_path / "bins"), "--no-dashboard"],
    )

    assert result.exit_code == 0, result.output
    assert cli.os.environ["HF_HUB_DOWNLOAD_TIMEOUT"] == "30"
    assert cli.os.environ["HF_HUB_ETAG_TIMEOUT"] == "30"


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
        raise ConfigurationError("bad roster")

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


def test_throttling_client_errors_are_retried(monkeypatch):
    calls = 0

    class ClientError(Exception):
        response = {"Error": {"Code": "SlowDown"}}

    class FakeTrainer:
        def run(self):
            pass

    def build(_resume):
        nonlocal calls
        calls += 1
        if calls == 1:
            raise ClientError("throttled")
        return FakeTrainer()

    monkeypatch.setattr("time.sleep", lambda _seconds: None)
    assert cli._run_until_done(build, resume=False, view=None).__class__ is FakeTrainer
    assert calls == 2
