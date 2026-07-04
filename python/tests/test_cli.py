import pytest
from typer.testing import CliRunner

from sngram.cli import _run_until_done, app


def test_cli_does_not_expose_synthetic_ingest_benchmark():
    result = CliRunner().invoke(app, ["--help"])
    assert result.exit_code == 0
    assert "bench-ingest" not in result.output
    assert "fs-validate" in result.output


def test_train_preflights_sources_before_running(monkeypatch, tmp_path):
    calls = []
    init_kwargs = []

    class FakeTrainer:
        def __init__(self, **kwargs):
            init_kwargs.append(kwargs)

        def preflight_sources(self):
            calls.append("preflight")

        async def run(self):
            calls.append("run")

        def describe_progress(self):
            return "ok"

    monkeypatch.setattr("sngram.train.config.hf_token", lambda: "hf_test")
    monkeypatch.setattr("sngram.train.config.default_families", lambda: [])
    monkeypatch.setattr("sngram.train.pipeline.Trainer", FakeTrainer)
    monkeypatch.setattr("sngram.train.pipeline.default_workers", lambda: 1)

    result = CliRunner().invoke(
        app,
        [
            "train",
            "--mint-dir",
            str(tmp_path / "bins"),
            "--limit",
            "1KB",
            "--no-dashboard",
        ],
    )

    assert result.exit_code == 0, result.output
    assert calls == ["preflight", "run"]
    assert init_kwargs[0]["target"] == 12_000_000_000_000


def test_train_preflight_failure_exits_without_retrying(monkeypatch, tmp_path):
    calls = []

    class FakeTrainer:
        def __init__(self, **_kwargs):
            pass

        def preflight_sources(self):
            calls.append("preflight")
            raise RuntimeError("missing source")

        async def run(self):
            calls.append("run")

        def describe_progress(self):
            return "ok"

    monkeypatch.setattr("sngram.train.config.hf_token", lambda: "hf_test")
    monkeypatch.setattr("sngram.train.config.default_families", lambda: [])
    monkeypatch.setattr("sngram.train.pipeline.Trainer", FakeTrainer)
    monkeypatch.setattr("sngram.train.pipeline.default_workers", lambda: 1)

    result = CliRunner().invoke(
        app,
        [
            "train",
            "--mint-dir",
            str(tmp_path / "bins"),
            "--limit",
            "1KB",
            "--no-dashboard",
        ],
    )

    assert result.exit_code == 2
    assert "preflight failed" in result.output
    assert calls == ["preflight"]


def test_preflight_failure_is_not_crash_retried(monkeypatch):
    calls = []

    class FakeTrainer:
        def preflight_sources(self):
            calls.append("preflight")
            raise RuntimeError("preflight failed")

        async def run(self):
            calls.append("run")

    def fail_if_retried(_seconds):
        raise AssertionError("preflight failure was retried")

    monkeypatch.setattr("time.sleep", fail_if_retried)

    with pytest.raises(RuntimeError, match="preflight failed"):
        _run_until_done(lambda _resume: FakeTrainer(), resume=False, dashboard=False)

    assert calls == ["preflight"]
