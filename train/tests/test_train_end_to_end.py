import json
from pathlib import Path

from typer.testing import CliRunner

from sngram_train import cli
from tests.localcorpus import code_repos, decoded_bytes, write_corpus

SHARDS = [code_repos(8) for _ in range(3)]


def patch_hub(monkeypatch, tmp_path: Path):
    corpus, source = write_corpus(tmp_path / "corpus", SHARDS)
    monkeypatch.setattr("sngram_train.corpus.resolve_corpus", lambda token: corpus)
    monkeypatch.setattr(
        "sngram_train.corpus.HubShards", lambda corpus, token: source
    )


def train_command(monkeypatch, tmp_path: Path, *arguments):
    patch_hub(monkeypatch, tmp_path)
    return CliRunner().invoke(
        cli.app,
        ["train", "--mint-dir", str(tmp_path / "bins"), "--workers", "3", *arguments],
    )


def events_of(tmp_path: Path, kind: str):
    path = tmp_path / "bins" / "train-events.jsonl"
    return [
        event
        for event in map(json.loads, path.read_text().splitlines())
        if event["kind"] == kind
    ]


def test_train_streams_the_corpus_and_mints_the_final_table(monkeypatch, tmp_path):
    result = train_command(monkeypatch, tmp_path, "--no-dashboard")

    assert result.exit_code == 0, result.output
    assert "done:" in result.output
    assert (tmp_path / "bins" / "final_weights.bin").exists()
    mints = events_of(tmp_path, "mint")
    assert mints[-1]["decoded"] == decoded_bytes(SHARDS)
    assert mints[-1]["languages"] == {"Rust": decoded_bytes(SHARDS)}


def test_train_command_renders_the_dashboard(monkeypatch, tmp_path):
    result = train_command(monkeypatch, tmp_path)

    assert result.exit_code == 0, result.output
    assert "done:" in result.output
    assert "sngram train" in result.output


def test_limit_bounds_a_smoke_run(monkeypatch, tmp_path):
    result = train_command(monkeypatch, tmp_path, "--limit", "1KB", "--no-dashboard")

    assert result.exit_code == 0, result.output
    summary = events_of(tmp_path, "summary")[-1]
    assert summary["decoded"] >= 1_000


def test_shards_bound_a_smoke_run(monkeypatch, tmp_path):
    result = train_command(monkeypatch, tmp_path, "--shards", "1", "--no-dashboard")

    assert result.exit_code == 0, result.output
    summary = events_of(tmp_path, "summary")[-1]
    assert summary["decoded"] == decoded_bytes(SHARDS[:1])


def test_completed_run_resumes_as_a_no_op(monkeypatch, tmp_path):
    first = train_command(monkeypatch, tmp_path, "--no-dashboard")
    assert first.exit_code == 0, first.output
    minted = (tmp_path / "bins" / "final_weights.bin").read_bytes()

    second = train_command(monkeypatch, tmp_path, "--no-dashboard")

    assert second.exit_code == 0, second.output
    assert (tmp_path / "bins" / "final_weights.bin").read_bytes() == minted


def test_train_without_a_readable_dataset_fails_with_guidance(monkeypatch, tmp_path):
    from sngram_train.errors import ConfigurationError

    def missing(token):
        raise ConfigurationError("cannot read dataset local/missing")

    monkeypatch.setattr("sngram_train.corpus.resolve_corpus", missing)
    result = CliRunner().invoke(
        cli.app,
        ["train", "--mint-dir", str(tmp_path / "bins"), "--no-dashboard"],
    )

    assert result.exit_code == 2
    assert "cannot read dataset" in result.output
