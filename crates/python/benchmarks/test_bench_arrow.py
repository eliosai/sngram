"""Benchmarks for the zero-copy Arrow counting path."""

import pyarrow as pa
import pytest

import sngram

ROWS = [
    "fn main() { println!(\"hello world\"); }",
    "pub async fn read_content(hash: Hash) -> Result<Bytes, Error> {}",
    "def max_file_size(limit):\n    return min(limit, MAX_FILE_SIZE)",
    "SELECT grams FROM content_ngrams WHERE grams @> ARRAY[1,2,3];",
    "the quick brown fox jumps over the lazy dog",
    "for (int i = 0; i < n; i++) { sum += values[i]; }",
]

ROW_COUNTS = [1_000, 20_000]


def corpus(row_count: int) -> list[str]:
    return [ROWS[index % len(ROWS)] for index in range(row_count)]


@pytest.fixture(scope="module", params=ROW_COUNTS, ids=[str(n) for n in ROW_COUNTS])
def arrow_table(request):
    return pa.table({"content": pa.array(corpus(request.param), type=pa.large_string())})


@pytest.fixture(scope="module")
def chunked_table():
    rows = corpus(20_000)
    chunks = [pa.array(rows[start : start + 2_000]) for start in range(0, len(rows), 2_000)]
    return pa.table({"content": pa.chunked_array(chunks)})


def test_count_arrow(benchmark, arrow_table):
    def count():
        counter = sngram.BigramCounter()
        counter.count_arrow(arrow_table)
        return counter

    counter = benchmark(count)
    assert counter.bytes_processed > 0


def test_count_arrow_chunked(benchmark, chunked_table):
    def count():
        counter = sngram.BigramCounter()
        counter.count_arrow(chunked_table)
        return counter

    counter = benchmark(count)
    assert counter.bytes_processed > 0


def test_counter_merge(benchmark, chunked_table):
    staging = sngram.BigramCounter()
    staging.count_arrow(chunked_table)

    def merge():
        counter = sngram.BigramCounter()
        counter.merge(staging)
        return counter

    counter = benchmark(merge)
    assert counter.bytes_processed > 0
