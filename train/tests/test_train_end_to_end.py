import gzip
import json
from pathlib import Path

from typer.testing import CliRunner

from sngram_train import cli

GROUPS = ["code", "config", "docs", "web", "data", "other"]
LINE = b"fn main() { return 42; }\n"
DOC = len(LINE) * 40
PER_GROUP = 50

CONTENT: dict[str, bytes] = {}


class FakeSwhContent:
    def __init__(self, workers=32):
        self.workers = workers

    def read(self, blob_id, _max_bytes):
        return CONTENT[blob_id]


def publish_fake_corpus(repo_dir: Path):
    (repo_dir / "data").mkdir(parents=True, exist_ok=True)
    rows = []
    for group in GROUPS:
        for index in range(PER_GROUP):
            blob = f"{group}-{index}"
            CONTENT[blob] = LINE * 40
            rows.append({
                "group": group, "language": "X", "extension": "x",
                "license": "permissive", "blob_id": blob,
                "encoding": "UTF-8", "length": DOC, "weight": 1,
            })
    with gzip.open(
        repo_dir / "data" / "train-00000-of-00001.jsonl.gz", "wt", encoding="utf-8"
    ) as handle:
        for row in rows:
            handle.write(json.dumps(row) + "\n")
    sidecar = {
        "revision": "rev-e2e",
        "rows": len(rows),
        "raw_bytes": len(rows) * DOC,
        "effective_bytes": len(rows) * DOC,
        "groups": {group: PER_GROUP * DOC for group in GROUPS},
    }
    (repo_dir / "manifest.json").write_text(json.dumps(sidecar))


def patch_hub(monkeypatch, tmp_path: Path):
    import sngram_train.content
    import sngram_train.stream

    repo_dir = tmp_path / "repo"
    if not repo_dir.exists():
        publish_fake_corpus(repo_dir)

    def load(_repo, _token):
        from datasets import load_dataset

        return load_dataset(
            "json", data_files=str(repo_dir / "data" / "*.jsonl.gz"),
            split="train", streaming=True,
        )

    monkeypatch.setattr(sngram_train.stream, "_load", load)
    monkeypatch.setattr(
        "huggingface_hub.hf_hub_download",
        lambda *_args, **_kwargs: str(repo_dir / "manifest.json"),
    )
    monkeypatch.setattr(sngram_train.content, "SwhContent", FakeSwhContent)


def train_command(monkeypatch, tmp_path: Path, *arguments):
    patch_hub(monkeypatch, tmp_path)
    return CliRunner().invoke(
        cli.app,
        ["train", "--mint-dir", str(tmp_path / "bins"), *arguments],
    )


def events_of(tmp_path: Path, kind: str):
    path = tmp_path / "bins" / "train-events.jsonl"
    return [
        event
        for event in map(json.loads, path.read_text().splitlines())
        if event["kind"] == kind
    ]


def test_train_streams_the_corpus_and_mints_the_final_table(monkeypatch, tmp_path):
    result = train_command(monkeypatch, tmp_path, "--no-dashboard", "--workers", "8")

    assert result.exit_code == 0, result.output
    assert "done:" in result.output
    assert (tmp_path / "bins" / "final_weights.bin").exists()
    mints = events_of(tmp_path, "mint")
    assert mints[-1]["effective_bytes"] == len(GROUPS) * PER_GROUP * DOC
    assert set(mints[-1]["groups"]) == set(GROUPS)


def test_train_command_renders_the_dashboard(monkeypatch, tmp_path):
    result = train_command(monkeypatch, tmp_path, "--workers", "8")

    assert result.exit_code == 0, result.output
    assert "done:" in result.output
    assert "sngram train" in result.output


def test_limit_bounds_a_smoke_run(monkeypatch, tmp_path):
    result = train_command(
        monkeypatch, tmp_path, "--limit", "10KB", "--no-dashboard", "--workers", "8"
    )

    assert result.exit_code == 0, result.output
    summary = events_of(tmp_path, "summary")[-1]
    assert 10_000 <= summary["effective_bytes"] < len(GROUPS) * PER_GROUP * DOC


def test_completed_run_resumes_as_a_no_op(monkeypatch, tmp_path):
    first = train_command(monkeypatch, tmp_path, "--no-dashboard", "--workers", "8")
    assert first.exit_code == 0, first.output

    second = train_command(monkeypatch, tmp_path, "--no-dashboard", "--workers", "8")

    assert second.exit_code == 0, second.output
    assert "done:" in second.output


def test_train_without_a_published_corpus_fails_with_guidance(monkeypatch, tmp_path):
    from huggingface_hub.errors import EntryNotFoundError

    def missing(*_args, **_kwargs):
        raise EntryNotFoundError("no manifest")

    monkeypatch.setattr("huggingface_hub.hf_hub_download", missing)
    result = CliRunner().invoke(
        cli.app,
        ["train", "--mint-dir", str(tmp_path / "bins"), "--no-dashboard"],
    )

    assert result.exit_code == 2
    assert "no published corpus sidecar" in result.output
