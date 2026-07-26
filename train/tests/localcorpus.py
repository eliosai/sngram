"""Local corpus fixtures shaped like the shards each layout streams."""

from __future__ import annotations

import gzip
import json
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

from sngram_train.corpus import Corpus, CorpusIdentity, Quota, Reading, Shard

REVISION = "rev-test"

STACK_READING = Reading("stack-files", "content")

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
    """Write nested stack shards and return the corpus over them."""

    (root / "data").mkdir(parents=True, exist_ok=True)
    listed = []
    for index, repos in enumerate(shards):
        name = f"data/part-{index:05d}.parquet"
        pq.write_table(_table(repos), root / name, row_group_size=rows_per_group)
        listed.append(
            Shard(name, (root / name).stat().st_size, STACK_READING, "stack-v3", "local")
        )
    identity = CorpusIdentity("stack-v3", REVISION)
    return Corpus(identity, tuple(listed)), LocalShards(root)


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


def write_column_shard(
    path: Path, field: str, values: list[str], rows_per_group: int = 4
) -> None:
    """Write one flat parquet shard carrying a single text column."""

    path.parent.mkdir(parents=True, exist_ok=True)
    table = pa.table({field: pa.array(values, type=pa.string()), "id": range(len(values))})
    pq.write_table(table, path, row_group_size=rows_per_group)


def write_line_shard(path: Path, field: str, records: list[dict]) -> None:
    """Write one gzipped JSON-lines shard."""

    path.parent.mkdir(parents=True, exist_ok=True)
    with gzip.open(path, "wt", encoding="utf-8") as handle:
        for record in records:
            handle.write(json.dumps(record) + "\n")


def blend_corpus(
    root: Path, sources: dict[str, list[list[str]]], caps: dict[str, int]
) -> tuple[Corpus, "LocalShards"]:
    """A blend over flat parquet shards, one family per source."""

    shards = []
    for source, files in sources.items():
        for index, rows in enumerate(files):
            name = f"{source}/part-{index:03d}.parquet"
            write_column_shard(root / name, "text", rows, rows_per_group=len(rows))
            size = (root / name).stat().st_size
            shards.append(
                Shard(name, size, Reading("parquet-column", "text"), source, source)
            )
    quota = Quota(dict(caps), dict(caps))
    identity = CorpusIdentity("blend", "fingerprint-test")
    return Corpus(identity, tuple(shards), quota), LocalShards(root)


def blend_rows(mark: str, shards: int, rows: int = 5, width: int = 10) -> list[list[str]]:
    """Uniform shards of equal-width rows, so a ceiling lands on a row boundary."""

    return [[mark * width] * rows for _ in range(shards)]


class LocalShards:
    """Shard opener over a local corpus directory."""

    def __init__(self, root: Path) -> None:
        self._root = root

    def open(self, shard: Shard):
        return open(self._root / shard.path, "rb")
