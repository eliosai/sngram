"""Stack v3 corpus identity and shard access."""

from __future__ import annotations

import os
from dataclasses import dataclass

from .errors import ConfigurationError

DEFAULT_REPO = "HuggingFaceCode/stack-v3-train"

_BLOCK_SIZE = 16 * 2**20


def corpus_repo() -> str:
    return os.environ.get("SNGRAM_DATASET_REPO", DEFAULT_REPO)


@dataclass(frozen=True)
class Shard:
    """One parquet shard file in the dataset"""

    name: str
    size: int


@dataclass(frozen=True)
class Corpus:
    """The resolved dataset: repo, pinned revision, shard listing"""

    repo: str
    revision: str
    shards: tuple[Shard, ...]

    def wire_bytes(self) -> int:
        return sum(shard.size for shard in self.shards)

    def take(self, count: int) -> Corpus:
        return Corpus(self.repo, self.revision, self.shards[:count])


def resolve_corpus(token: str | None) -> Corpus:
    """Pin the dataset revision and list its parquet shards with sizes."""

    from huggingface_hub import HfApi
    from huggingface_hub.errors import GatedRepoError, RepositoryNotFoundError

    try:
        info = HfApi(token=token).dataset_info(corpus_repo(), files_metadata=True)
    except (RepositoryNotFoundError, GatedRepoError) as error:
        raise ConfigurationError(f"cannot read dataset {corpus_repo()}") from error
    shards = tuple(
        Shard(entry.rfilename, entry.size or 0)
        for entry in sorted(info.siblings, key=lambda entry: entry.rfilename)
        if entry.rfilename.startswith("data/") and entry.rfilename.endswith(".parquet")
    )
    if not shards:
        raise ConfigurationError(f"{corpus_repo()} has no parquet shards under data/")
    return Corpus(corpus_repo(), info.sha, shards)


class HubShards:
    """Random access shard reads over the Hub filesystem."""

    def __init__(self, corpus: Corpus, token: str | None) -> None:
        from huggingface_hub import HfFileSystem

        self._prefix = f"datasets/{corpus.repo}@{corpus.revision}"
        self._fs = HfFileSystem(token=token)

    def open(self, name: str):
        return self._fs.open(f"{self._prefix}/{name}", "rb", block_size=_BLOCK_SIZE)
