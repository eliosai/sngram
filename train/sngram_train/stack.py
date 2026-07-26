"""The Stack v3 corpus: one repo of nested repository shards."""

from __future__ import annotations

import os

from . import hub
from .corpus import Corpus, CorpusIdentity, Reading, Shard
from .errors import ConfigurationError

DEFAULT_REPO = "HuggingFaceCode/stack-v3-train"

NAME = "stack-v3"

FAMILY = "stack-v3"

READING = Reading("stack-files", "content")


def corpus_repo() -> str:
    return os.environ.get("SNGRAM_DATASET_REPO", DEFAULT_REPO)


def resolve(token: str | None, note=None) -> Corpus:
    """Pin the dataset revision and list its parquet shards with sizes."""

    repo = corpus_repo()
    listing = hub.listing(repo, token, note)
    shards = tuple(
        Shard(listing.hub_path(path), size, READING, FAMILY, repo)
        for path, size in listing.under("data/", ".parquet")
    )
    if not shards:
        raise ConfigurationError(f"{repo} has no parquet shards under data/")
    return Corpus(CorpusIdentity(NAME, listing.revision), shards)
