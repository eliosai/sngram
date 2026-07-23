import gzip
import json
from pathlib import Path

from typer.testing import CliRunner

from sngram_train import cli
from sngram_train.catalog import build_catalog
from sngram_train.config import GROUP_LABELS, STACK_V2_REVISION

CONFIGS = ["HTML", "JSON", "Markdown", "Python", "SQL", "Weird"]
LINE = b"fn main() { return 42; }\n"
DOC = len(LINE) * 800
PER_CONFIG = 120

CONTENT: dict[str, bytes] = {}


class FakeSwhContent:
    def __init__(self, workers=32):
        self.workers = workers

    def read(self, blob_id, _max_bytes):
        return CONTENT[blob_id]


def publish_fake_dataset(repo_dir: Path):
    catalog = build_catalog(CONFIGS)
    (repo_dir / "data").mkdir(parents=True, exist_ok=True)
    with gzip.open(
        repo_dir / "data" / "train-00000-of-00001.jsonl.gz", "wt", encoding="utf-8"
    ) as handle:
        for item in catalog.formats:
            for index in range(PER_CONFIG):
                blob = f"{item.config}-{index}"
                CONTENT[blob] = LINE * 800
                handle.write(json.dumps(_row(item, blob)) + "\n")
    (repo_dir / "manifest.json").write_text(json.dumps(_sidecar(catalog)))


def _row(item, blob):
    return {
        "group": GROUP_LABELS[item.area],
        "language": item.config,
        "extension": "x",
        "license": "permissive",
        "blob_id": blob,
        "encoding": "UTF-8",
        "length": DOC,
        "weight": 1,
    }


def _sidecar(catalog):
    return {
        "revision": STACK_V2_REVISION,
        "roster_hash": catalog.roster_hash(STACK_V2_REVISION),
        "built_target": None,
        "effective_target": len(catalog.formats) * PER_CONFIG * DOC,
        "formats": [
            {"id": item.id, "candidates": PER_CONFIG, "exhausted": True}
            for item in catalog.formats
        ],
    }


def patch_hub(monkeypatch, tmp_path: Path):
    import sngram_train.assets
    import sngram_train.content

    repo_dir = tmp_path / "repo"
    if not repo_dir.exists():
        publish_fake_dataset(repo_dir)
    monkeypatch.setattr(
        sngram_train.assets, "_snapshot", lambda _repo, _token: str(repo_dir)
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


def test_train_fetches_the_dataset_and_mints_the_final_table(monkeypatch, tmp_path):
    result = train_command(
        monkeypatch, tmp_path, "--limit", "600KB", "--no-dashboard", "--workers", "8"
    )

    assert result.exit_code == 0, result.output
    assert "fetching manifest dataset" in result.output
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
    result = train_command(monkeypatch, tmp_path, "--limit", "600KB", "--workers", "8")

    assert result.exit_code == 0, result.output
    assert "done:" in result.output
    assert (tmp_path / "bins" / "final_weights.bin").exists()
    assert "sngram train" in result.output


def test_second_train_reuses_the_imported_manifest(monkeypatch, tmp_path):
    first = train_command(
        monkeypatch, tmp_path, "--limit", "600KB", "--no-dashboard", "--workers", "8"
    )
    assert first.exit_code == 0, first.output

    second = train_command(
        monkeypatch, tmp_path, "--limit", "600KB", "--no-dashboard", "--workers", "8"
    )

    assert second.exit_code == 0, second.output
    assert "fetching manifest dataset" not in second.output


def test_train_clamps_an_infeasible_target_with_a_warning(monkeypatch, tmp_path):
    result = train_command(
        monkeypatch, tmp_path, "--limit", "500MB", "--no-dashboard", "--workers", "8"
    )

    assert result.exit_code == 0, result.output
    assert "corpus supplies" in result.output
    assert "done:" in result.output
    assert (tmp_path / "bins" / "final_weights.bin").exists()
    summary = events_of(tmp_path, "summary")[-1]
    assert summary["complete"] is True


def test_train_without_a_published_dataset_fails_with_guidance(monkeypatch, tmp_path):
    import sngram_train.assets

    empty = tmp_path / "empty"
    empty.mkdir()
    monkeypatch.setattr(
        sngram_train.assets, "_snapshot", lambda _repo, _token: str(empty)
    )
    result = CliRunner().invoke(
        cli.app,
        ["train", "--mint-dir", str(tmp_path / "bins"), "--no-dashboard"],
    )

    assert result.exit_code == 2
    assert "no published manifest sidecar" in result.output
