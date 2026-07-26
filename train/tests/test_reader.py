from pathlib import Path

import pytest

from sngram_train.corpus import Reading, Shard
from sngram_train.errors import ConfigurationError
from sngram_train.reader import ColumnReader, LineReader, StackReader, open_reader
from tests.localcorpus import (
    repo,
    write_column_shard,
    write_corpus,
    write_line_shard,
)


def build_reader(tmp_path: Path, shards, rows_per_group=2) -> StackReader:
    corpus, source = write_corpus(tmp_path, shards, rows_per_group)
    return StackReader(source.open(corpus.shards[0]))


def read_all(reader) -> list:
    return [sifted for group in range(reader.row_groups) for sifted in reader.batches(group, 0)]


def texts(sifted_list) -> list[str]:
    return [
        value
        for sifted in sifted_list
        for value in sifted.content.column(0).to_pylist()
    ]


def test_batches_carry_all_content_and_language_bytes(tmp_path: Path):
    shard = [
        repo(("héllo\n", "Python", False), ("vendored", "JavaScript", True)),
        repo(("fn main() {}\n", "Rust", False), ("SELECT 1;\n", "SQL", False)),
    ]
    reader = build_reader(tmp_path, [shard], rows_per_group=4)

    batches = list(reader.batches(0, 0))

    assert len(batches) == 1
    sifted = batches[0]
    assert sifted.rows == 2
    assert sifted.files == 4
    assert sifted.vendor_files == 1
    assert sifted.lang_bytes == {"Python": 7, "JavaScript": 8, "Rust": 13, "SQL": 10}
    assert sifted.content.num_rows == 4


def test_null_content_is_dropped(tmp_path: Path):
    shard = [
        [
            {"content": None, "language": "Python", "is_vendor": False, "size_bytes": None},
            {"content": "ok\n", "language": "Python", "is_vendor": False, "size_bytes": 3},
        ]
    ]
    reader = build_reader(tmp_path, [shard], rows_per_group=1)

    sifted = next(reader.batches(0, 0))

    assert sifted.files == 1
    assert sifted.lang_bytes == {"Python": 3}


def test_skip_resumes_at_a_batch_boundary(tmp_path: Path):
    shard = [repo((f"row {index}\n", "Rust", False)) for index in range(130)]
    reader = build_reader(tmp_path, [shard], rows_per_group=200)

    full = [sifted.rows for sifted in reader.batches(0, 0)]
    resumed = [sifted.rows for sifted in reader.batches(0, 64)]

    assert full == [64, 64, 2]
    assert resumed == [64, 2]


def test_offset_batches_carry_their_own_rows(tmp_path: Path):
    import sngram

    shard = [repo((f"row {index:03d}\n", "Rust", False)) for index in range(130)]
    reader = build_reader(tmp_path, [shard], rows_per_group=200)

    counter = sngram.BigramCounter()
    for sifted in reader.batches(0, 0):
        staged = sngram.BigramCounter()
        staged.count_arrow(sifted.content)
        counter.merge(staged)

    expected = sum(len(f"row {index:03d}\n") for index in range(130))
    assert counter.bytes_processed == expected


def test_misaligned_skip_fails_loudly(tmp_path: Path):
    shard = [repo(("data\n", "Rust", False)) for _ in range(4)]
    reader = build_reader(tmp_path, [shard], rows_per_group=2)

    with pytest.raises(ConfigurationError, match="align"):
        list(reader.batches(0, 1))


def test_row_groups_follow_the_write_layout(tmp_path: Path):
    shard = [repo(("data\n", "Rust", False)) for _ in range(6)]
    reader = build_reader(tmp_path, [shard], rows_per_group=2)

    assert reader.row_groups == 3


def column_shard(tmp_path: Path, field: str, values: list[str], rows_per_group=4) -> Shard:
    path = tmp_path / "flat.parquet"
    write_column_shard(path, field, values, rows_per_group)
    return Shard(str(path), path.stat().st_size, Reading("parquet-column", field), "f", "s")


def line_shard(tmp_path: Path, field: str, records: list[dict]) -> Shard:
    path = tmp_path / "lines.json.gz"
    write_line_shard(path, field, records)
    return Shard(str(path), path.stat().st_size, Reading("json-lines", field), "f", "s")


def test_a_flat_shard_yields_only_its_text_column(tmp_path: Path):
    shard = column_shard(tmp_path, "text", ["alpha", "beta", "gamma"], rows_per_group=2)

    with open(shard.path, "rb") as handle:
        reader = open_reader(handle, shard)
        assert isinstance(reader, ColumnReader)
        assert reader.row_groups == 2
        batches = read_all(reader)

    assert texts(batches) == ["alpha", "beta", "gamma"]
    assert [sifted.rows for sifted in batches] == [2, 1]
    assert all(sifted.lang_bytes == {} for sifted in batches)


def test_each_source_reads_its_own_text_field(tmp_path: Path):
    for field in ("content", "text", "Body"):
        shard = column_shard(tmp_path, field, [f"{field} value"])
        with open(shard.path, "rb") as handle:
            assert texts(read_all(open_reader(handle, shard))) == [f"{field} value"]


def test_a_flat_shard_without_the_text_column_fails_loudly(tmp_path: Path):
    shard = column_shard(tmp_path, "text", ["value"])
    wrong = Shard(shard.path, shard.size, Reading("parquet-column", "Body"), "f", "s")

    with open(wrong.path, "rb") as handle:
        with pytest.raises(ConfigurationError, match="no 'Body' column"):
            open_reader(handle, wrong)


def test_null_column_values_are_dropped_from_a_flat_shard(tmp_path: Path):
    path = tmp_path / "flat.parquet"
    import pyarrow as pa
    import pyarrow.parquet as pq

    pq.write_table(pa.table({"text": pa.array(["kept", None])}), path, row_group_size=4)
    shard = Shard(str(path), 1, Reading("parquet-column", "text"), "f", "s")

    with open(path, "rb") as handle:
        batches = read_all(open_reader(handle, shard))

    assert texts(batches) == ["kept"]
    assert batches[0].rows == 2, "skipped rows still count toward resume"
    assert batches[0].files == 1


def test_a_gzipped_json_shard_reads_its_field_as_one_row_group(tmp_path: Path):
    shard = line_shard(
        tmp_path,
        "content",
        [{"content": "one", "path": "a"}, {"content": "two", "path": "b"}],
    )

    with open(shard.path, "rb") as handle:
        reader = open_reader(handle, shard)
        assert isinstance(reader, LineReader)
        assert reader.row_groups == 1
        batches = list(reader.batches(0, 0))

    assert texts(batches) == ["one", "two"]
    assert sum(sifted.rows for sifted in batches) == 2


def test_json_lines_missing_the_field_still_advance_the_row_cursor(tmp_path: Path):
    shard = line_shard(
        tmp_path, "content", [{"other": 1}, {"content": "kept"}, {"content": None}]
    )

    with open(shard.path, "rb") as handle:
        batches = list(open_reader(handle, shard).batches(0, 0))

    assert texts(batches) == ["kept"]
    assert sum(sifted.rows for sifted in batches) == 3


def test_a_resumed_json_shard_skips_the_lines_already_counted(tmp_path: Path):
    records = [{"content": f"line {index}"} for index in range(5)]
    shard = line_shard(tmp_path, "content", records)

    with open(shard.path, "rb") as handle:
        batches = list(open_reader(handle, shard).batches(0, 3))

    assert texts(batches) == ["line 3", "line 4"]
    assert sum(sifted.rows for sifted in batches) == 2
