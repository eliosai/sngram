import gzip
import json
from pathlib import Path

import pytest

from sngram_train import stream
from sngram_train.errors import ConfigurationError

ROWS = [
    {"group": "code", "language": "Python", "extension": "py", "license": "permissive",
     "blob_id": f"b{index}", "encoding": "UTF-8", "length": 100 + index, "weight": 1 + index % 3}
    for index in range(10)
]


def publish(tmp_path: Path):
    data = tmp_path / "repo" / "data"
    data.mkdir(parents=True, exist_ok=True)
    halves = [ROWS[:5], ROWS[5:]]
    for index, half in enumerate(halves):
        name = f"train-{index:05d}-of-{len(halves):05d}.jsonl.gz"
        with gzip.open(data / name, "wt", encoding="utf-8") as handle:
            for item in half:
                handle.write(json.dumps(item) + "\n")
    sidecar = {
        "revision": "rev-1",
        "corpus_id": "corpus-rev-1",
        "rows": len(ROWS),
        "raw_bytes": sum(item["length"] for item in ROWS),
        "effective_bytes": sum(item["length"] * item["weight"] for item in ROWS),
        "groups": {"code": sum(item["length"] * item["weight"] for item in ROWS)},
    }
    (tmp_path / "repo" / "manifest.json").write_text(json.dumps(sidecar))
    return tmp_path / "repo"


def local_loader(repo_dir: Path):
    def load(_repo, _token):
        from datasets import load_dataset

        return load_dataset(
            "json", data_files=str(repo_dir / "data" / "*.jsonl.gz"),
            split="train", streaming=True,
        )

    return load


def test_stream_yields_typed_rows_in_order(tmp_path: Path, monkeypatch):
    monkeypatch.setattr(stream, "_load", local_loader(publish(tmp_path)))

    rows = list(stream.CorpusStream.open(token=None))

    assert [row.blob_id for row in rows] == [item["blob_id"] for item in ROWS]
    assert rows[0] == stream.CorpusRow("code", "b0", "UTF-8", 100, 1)


def test_stream_state_resumes_where_it_stopped(tmp_path: Path, monkeypatch):
    monkeypatch.setattr(stream, "_load", local_loader(publish(tmp_path)))

    first = stream.CorpusStream.open(token=None)
    iterator = iter(first)
    consumed = [next(iterator).blob_id for _ in range(7)]
    state = first.state_dict()

    resumed = stream.CorpusStream.open(token=None, state=state)
    remaining = [row.blob_id for row in resumed]

    assert consumed + remaining == [item["blob_id"] for item in ROWS]


def test_corpus_meta_reads_the_sidecar(tmp_path: Path, monkeypatch):
    repo = publish(tmp_path)
    monkeypatch.setattr(
        "huggingface_hub.hf_hub_download",
        lambda *_args, **_kwargs: str(repo / "manifest.json"),
    )

    meta = stream.corpus_meta(token=None)

    assert meta.revision == "rev-1"
    assert meta.corpus_id == "corpus-rev-1"
    assert meta.rows == 10
    assert meta.groups["code"] == meta.effective_bytes


def test_missing_sidecar_fails_with_guidance(monkeypatch):
    from huggingface_hub.errors import EntryNotFoundError

    def missing(*_args, **_kwargs):
        raise EntryNotFoundError("no manifest")

    monkeypatch.setattr("huggingface_hub.hf_hub_download", missing)

    with pytest.raises(ConfigurationError, match="sidecar"):
        stream.corpus_meta(token=None)


def test_corpus_repo_honours_the_environment(monkeypatch):
    monkeypatch.setenv("SNGRAM_ASSETS_REPO", "other/repo")
    assert stream.corpus_repo() == "other/repo"
    monkeypatch.delenv("SNGRAM_ASSETS_REPO")
    assert stream.corpus_repo() == stream.DEFAULT_CORPUS_REPO
