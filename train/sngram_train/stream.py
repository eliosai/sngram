"""Streaming corpus rows from the published Hub dataset."""

from __future__ import annotations

import json
import os
from dataclasses import dataclass
from pathlib import Path

from .errors import ConfigurationError

DEFAULT_CORPUS_REPO = "eliosai/sngram-train"
MANIFEST_META = "manifest.json"


def corpus_repo() -> str:
    return os.environ.get("SNGRAM_ASSETS_REPO", DEFAULT_CORPUS_REPO)


@dataclass(frozen=True)
class CorpusMeta:
    revision: str
    corpus_id: str
    rows: int
    raw_bytes: int
    effective_bytes: int
    groups: dict[str, int]


@dataclass(frozen=True)
class CorpusRow:
    group: str
    blob_id: str
    encoding: str
    length: int
    weight: int


def corpus_meta(token: str | None) -> CorpusMeta:
    """Fetch the corpus sidecar from the Hub."""

    from huggingface_hub import hf_hub_download
    from huggingface_hub.errors import EntryNotFoundError, RepositoryNotFoundError

    try:
        path = hf_hub_download(
            corpus_repo(), MANIFEST_META, repo_type="dataset", token=token
        )
    except (RepositoryNotFoundError, EntryNotFoundError) as error:
        raise ConfigurationError(
            f"{corpus_repo()} has no published corpus sidecar"
        ) from error
    data = json.loads(Path(path).read_text())
    return CorpusMeta(
        data["revision"],
        data["corpus_id"],
        data["rows"],
        data["raw_bytes"],
        data["effective_bytes"],
        dict(data["groups"]),
    )


class CorpusStream:
    """Resumable ordered row stream over the published corpus."""

    def __init__(self, dataset) -> None:
        self._dataset = dataset

    @classmethod
    def open(cls, token: str | None, state: dict | None = None) -> CorpusStream:
        dataset = _load(corpus_repo(), token)
        if state is not None:
            dataset.load_state_dict(state)
        return cls(dataset)

    def __iter__(self):
        for item in self._dataset:
            yield CorpusRow(
                item["group"],
                item["blob_id"],
                item["encoding"],
                item["length"],
                item["weight"],
            )

    def state_dict(self) -> dict:
        return self._dataset.state_dict()


def _load(repo: str, token: str | None):
    from datasets import load_dataset

    return load_dataset(repo, split="train", streaming=True, token=token)
