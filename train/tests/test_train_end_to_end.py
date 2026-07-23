import json
from pathlib import Path

from typer.testing import CliRunner

from sngram_train import cli
from sngram_train.config import STACK_V2_REVISION

CONFIGS = ["HTML", "JSON", "Markdown", "Python", "SQL", "Weird"]
LINE = b"fn main() { return 42; }\n"
DOC = len(LINE) * 800

CONTENT: dict[str, bytes] = {}


class FakeHuggingFaceRows:
    def __init__(self, _token, revision=None):
        self.revision = revision or STACK_V2_REVISION

    def configs(self):
        return list(CONFIGS)

    def iter_rows(self, config, cursor=(0, 0)):
        for index in range(cursor[1], 120):
            blob = f"{config}-{index}"
            CONTENT[blob] = LINE * 800
            yield {
                "blob_id": blob,
                "content_id": blob,
                "src_encoding": "utf-8",
                "language": config,
                "path": f"/{config}/{index}",
                "extension": "x",
                "is_vendor": False,
                "is_generated": False,
                "length_bytes": DOC,
                "_sample_weight": 1,
                "_source_cursor": (0, index + 1),
            }


class FakeSwhContent:
    def __init__(self, workers=32):
        self.workers = workers

    def read(self, blob_id, _max_bytes):
        return CONTENT[blob_id]


def patch_stack(monkeypatch):
    import sngram_train.content
    import sngram_train.publishing
    import sngram_train.resources
    import sngram_train.stackrows

    monkeypatch.setattr(cli, "hf_token", lambda: "token")
    monkeypatch.setattr(sngram_train.publishing, "hf_token", lambda: "token")
    monkeypatch.setattr(sngram_train.resources, "MANIFEST_RESERVE_BYTES", 0)
    monkeypatch.setattr(sngram_train.stackrows, "HuggingFaceRows", FakeHuggingFaceRows)
    monkeypatch.setattr(sngram_train.content, "SwhContent", FakeSwhContent)


def build_command(monkeypatch, tmp_path: Path, *arguments):
    patch_stack(monkeypatch)
    return CliRunner().invoke(
        cli.app,
        ["manifest", "build", "--mint-dir", str(tmp_path / "bins"), *arguments],
    )


def train_command(monkeypatch, tmp_path: Path, *arguments):
    patch_stack(monkeypatch)
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


def test_build_then_train_runs_the_full_pipeline_headless(monkeypatch, tmp_path):
    built = build_command(monkeypatch, tmp_path, "--no-dashboard")
    assert built.exit_code == 0, built.output
    assert "manifest 6/6" in built.output

    result = train_command(
        monkeypatch, tmp_path, "--limit", "600KB", "--no-dashboard", "--workers", "8"
    )

    assert result.exit_code == 0, result.output
    assert "done:" in result.output
    assert (tmp_path / "bins" / "final_weights.bin").exists()
    mints = events_of(tmp_path, "mint")
    assert mints[-1]["effective_bytes"] == 600_000
    assert set(mints[-1]["areas"]) == {
        "config-build-infra",
        "core-programming",
        "data-query-schema",
        "docs-prose-markup",
        "long-tail",
        "web-ui-templates",
    }


def test_train_command_renders_the_dashboard(monkeypatch, tmp_path):
    built = build_command(monkeypatch, tmp_path, "--no-dashboard")
    assert built.exit_code == 0, built.output

    result = train_command(monkeypatch, tmp_path, "--limit", "600KB", "--workers", "8")

    assert result.exit_code == 0, result.output
    assert "done:" in result.output
    assert (tmp_path / "bins" / "final_weights.bin").exists()
    assert "sngram train" in result.output


def test_second_build_reuses_the_finished_manifest(monkeypatch, tmp_path):
    first = build_command(monkeypatch, tmp_path, "--no-dashboard")
    assert first.exit_code == 0, first.output

    second = build_command(monkeypatch, tmp_path, "--no-dashboard")

    assert second.exit_code == 0, second.output
    assert "already built" in second.output


def test_train_command_clamps_an_infeasible_target_with_a_warning(monkeypatch, tmp_path):
    built = build_command(monkeypatch, tmp_path, "--no-dashboard")
    assert built.exit_code == 0, built.output

    result = train_command(
        monkeypatch, tmp_path, "--limit", "500MB", "--no-dashboard", "--workers", "8"
    )

    assert result.exit_code == 0, result.output
    assert "warning: corpus supplies" in result.output
    assert "done:" in result.output
    assert (tmp_path / "bins" / "final_weights.bin").exists()
    summary = events_of(tmp_path, "summary")[-1]
    assert summary["complete"] is True


def test_train_without_a_manifest_or_asset_fails_with_guidance(monkeypatch, tmp_path):
    import sngram_train.assets
    from sngram_train.errors import ConfigurationError

    def missing(_destination, _token):
        raise ConfigurationError(
            "eliosai/sngram-train has no published manifest; "
            "run `sngram manifest build --publish` once"
        )

    patch_stack(monkeypatch)
    monkeypatch.setattr(sngram_train.assets, "fetch_dataset", missing)
    result = CliRunner().invoke(
        cli.app,
        ["train", "--mint-dir", str(tmp_path / "bins"), "--no-dashboard"],
    )

    assert result.exit_code == 2
    assert "no published manifest" in result.output


def test_train_fetches_the_published_dataset_when_missing(monkeypatch, tmp_path):
    import shutil

    import sngram_train.assets

    repo_dir = tmp_path / "repo"

    def fake_upload(repo, folder, token):
        shutil.copytree(folder, repo_dir, dirs_exist_ok=True)

    def fake_snapshot(repo, token):
        return str(repo_dir)

    monkeypatch.setattr(sngram_train.assets, "_upload_folder", fake_upload)
    monkeypatch.setattr(sngram_train.assets, "_snapshot", fake_snapshot)

    built = build_command(monkeypatch, tmp_path / "publisher", "--no-dashboard", "--publish")
    assert built.exit_code == 0, built.output
    assert (repo_dir / "data").exists()

    result = train_command(
        monkeypatch,
        tmp_path / "reader",
        "--limit",
        "600KB",
        "--no-dashboard",
        "--workers",
        "8",
    )

    assert result.exit_code == 0, result.output
    assert "fetching manifest dataset" in result.output
    assert "done:" in result.output
    assert (tmp_path / "reader" / "bins" / "final_weights.bin").exists()
