"""Hugging Face Hub access: pinned dataset listings and shard reads."""

from __future__ import annotations

import time
from dataclasses import dataclass

from .errors import ConfigurationError, is_transient

_BLOCK_SIZE = 8 * 2**20

# seconds before one listing attempt is abandoned
_LISTING_TIMEOUT = 30.0

_LISTING_ATTEMPTS = 5

_LISTING_BACKOFF = 3.0


@dataclass(frozen=True)
class Listing:
    """One repo pinned at a commit, with every file and its wire size"""

    repo: str
    revision: str
    files: tuple[tuple[str, int], ...]

    def under(self, prefix: str, suffix: str) -> list[tuple[str, int]]:
        """Files below a path prefix, in listing order"""

        return sorted(
            (path, size)
            for path, size in self.files
            if path.startswith(prefix) and path.endswith(suffix)
        )

    def hub_path(self, path: str) -> str:
        return f"datasets/{self.repo}@{self.revision}/{path}"


def listing(repo: str, token: str | None, note=None) -> Listing:
    """Pin a dataset revision and list its files with sizes."""

    info = _dataset_info(repo, token, note)
    files = tuple(
        (entry.rfilename, entry.size or 0)
        for entry in sorted(info.siblings, key=lambda entry: entry.rfilename)
    )
    return Listing(repo, info.sha, files)


def _dataset_info(repo: str, token: str | None, note):
    """Fetch the listing, bounding every attempt so a stalled socket cannot hang."""

    from huggingface_hub import HfApi
    from huggingface_hub.errors import GatedRepoError, RepositoryNotFoundError

    api = HfApi(token=token)
    for attempt in range(_LISTING_ATTEMPTS):
        if note is not None:
            note(f"listing {repo} shards (attempt {attempt + 1})")
        try:
            return api.dataset_info(repo, files_metadata=True, timeout=_LISTING_TIMEOUT)
        except (RepositoryNotFoundError, GatedRepoError) as error:
            raise ConfigurationError(f"cannot read dataset {repo}") from error
        except Exception as error:
            if not is_transient(error) or attempt + 1 == _LISTING_ATTEMPTS:
                raise
            if note is not None:
                note(f"listing failed ({error}); retrying")
            time.sleep(_LISTING_BACKOFF * (attempt + 1))
    raise ConfigurationError(f"cannot list {repo} shards")


class HubShards:
    """Random access shard reads over the Hub filesystem."""

    def __init__(self, token: str | None) -> None:
        from huggingface_hub import HfFileSystem

        self._fs = HfFileSystem(token=token)

    def open(self, shard):
        return self._fs.open(shard.path, "rb", block_size=_BLOCK_SIZE)
