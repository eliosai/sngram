"""Local parquet corpus fixtures shaped like stack-v3 shards."""

from __future__ import annotations

from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

from sngram_train.corpus import Corpus, Shard

REVISION = "rev-test"

_FILES_TYPE = pa.list_(
    pa.struct(
        [
            ("content", pa.string()),
            ("language", pa.string()),
            ("is_vendor", pa.bool_()),
            ("size_bytes", pa.int64()),
        ]
    )
)


def repo(*files: tuple[str, str, bool]) -> list[dict]:
    """One repository row from (content, language, is_vendor) file specs."""

    return [
        {
            "content": content,
            "language": language,
            "is_vendor": vendor,
            "size_bytes": len(content.encode()) if content is not None else None,
        }
        for content, language, vendor in files
    ]


def code_repos(count: int, files_per_repo: int = 3) -> list[list[dict]]:
    """Uniform little Rust repositories for volume."""

    line = "fn main() { return 42; }\n"
    return [
        repo(*[(line * (4 + index % 3), "Rust", False) for index in range(files_per_repo)])
        for _ in range(count)
    ]


def write_corpus(
    root: Path, shards: list[list[list[dict]]], rows_per_group: int = 2
) -> tuple[Corpus, "LocalShards"]:
    """Write shard parquet files and return the corpus over them."""

    (root / "data").mkdir(parents=True, exist_ok=True)
    listed = []
    for index, repos in enumerate(shards):
        name = f"data/part-{index:05d}.parquet"
        pq.write_table(_table(repos), root / name, row_group_size=rows_per_group)
        listed.append(Shard(name, (root / name).stat().st_size))
    return Corpus("local/stack-v3-test", REVISION, tuple(listed)), LocalShards(root)


def _table(repos: list[list[dict]]) -> pa.Table:
    return pa.table(
        {
            "repo_path": pa.array([f"r/{index}" for index in range(len(repos))]),
            "github_metadata": pa.array(
                [{"stars": 5, "note": "never counted"}] * len(repos)
            ),
            "files": pa.array(repos, type=_FILES_TYPE),
        }
    )


def decoded_bytes(shards: list[list[list[dict]]]) -> int:
    """Expected countable bytes: all present content, UTF-8."""

    return sum(
        len(entry["content"].encode())
        for repos in shards
        for files in repos
        for entry in files
        if entry["content"] is not None
    )


class LocalShards:
    """Shard opener over a local corpus directory."""

    def __init__(self, root: Path) -> None:
        self._root = root

    def open(self, name: str):
        return open(self._root / name, "rb")
