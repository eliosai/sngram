from pathlib import Path

import pytest

from sngram_train.errors import ConfigurationError
from sngram_train.reader import ShardReader
from tests.localcorpus import repo, write_corpus


def build_reader(tmp_path: Path, shards, rows_per_group=2) -> ShardReader:
    corpus, source = write_corpus(tmp_path, shards, rows_per_group)
    return ShardReader(source.open(corpus.shards[0].name))


def test_batches_carry_filtered_content_and_language_bytes(tmp_path: Path):
    shard = [
        repo(("héllo\n", "Python", False), ("vendored", "JavaScript", True)),
        repo(("fn main() {}\n", "Rust", False), ("SELECT 1;\n", "SQL", False)),
    ]
    reader = build_reader(tmp_path, [shard], rows_per_group=4)

    batches = list(reader.batches(0, 0))

    assert len(batches) == 1
    sifted = batches[0]
    assert sifted.repos == 2
    assert sifted.files == 3
    assert sifted.vendor_files == 1
    assert sifted.lang_bytes == {"Python": 7, "Rust": 13, "SQL": 10}
    assert sifted.content.num_rows == 3


def test_null_content_is_dropped(tmp_path: Path):
    shard = [
        [
            {"content": None, "language": "Python", "is_vendor": False},
            {"content": "ok\n", "language": "Python", "is_vendor": False},
        ]
    ]
    reader = build_reader(tmp_path, [shard], rows_per_group=1)

    sifted = next(reader.batches(0, 0))

    assert sifted.files == 1
    assert sifted.lang_bytes == {"Python": 3}


def test_skip_resumes_at_a_batch_boundary(tmp_path: Path):
    shard = [repo((f"row {index}\n", "Rust", False)) for index in range(130)]
    reader = build_reader(tmp_path, [shard], rows_per_group=200)

    full = [sifted.repos for sifted in reader.batches(0, 0)]
    resumed = [sifted.repos for sifted in reader.batches(0, 64)]

    assert full == [64, 64, 2]
    assert resumed == [64, 2]


def test_misaligned_skip_fails_loudly(tmp_path: Path):
    shard = [repo(("data\n", "Rust", False)) for _ in range(4)]
    reader = build_reader(tmp_path, [shard], rows_per_group=2)

    with pytest.raises(ConfigurationError, match="align"):
        list(reader.batches(0, 1))


def test_row_groups_follow_the_write_layout(tmp_path: Path):
    shard = [repo(("data\n", "Rust", False)) for _ in range(6)]
    reader = build_reader(tmp_path, [shard], rows_per_group=2)

    assert reader.row_groups == 3
