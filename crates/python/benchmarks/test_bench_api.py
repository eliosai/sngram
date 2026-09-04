"""Benchmarks for the Python bindings: scanning, planning, and counting."""

import pytest

import sngram

SOURCE = (
    b"pub async fn read_content(hash: Hash) -> Result<Bytes, Error> {\n"
    b"    let limit = MAX_FILE_SIZE_LIMIT_EXCEEDED;\n"
    b"    let x = foo_bar(42);\n"
    b'    println!("{x}");\n'
    b"}\n"
)

PROSE = b"The quick brown fox jumps over the lazy dog. "

PATTERNS = [
    r"MAX_FILE",
    r"MAX_FILE_SIZE_LIMIT_EXCEEDED",
    r"MAX_[A-Z]+_SIZE",
    r"(foo|bar|baz)_handler",
    r"/usr/local/.*\.conf",
    r"(?i)quick brown fox",
    r"pub async fn \w+",
    r"ScanEvent::(Gram|Finish)",
    r"[0-9a-f]{8}-[0-9a-f]{4}",
    r"\berrors?\b",
]

SCAN_SIZES = [4096, 65536, 1048576]
COUNT_SIZES = [65536, 1048576]


def repeated(seed: bytes, size: int) -> bytes:
    return (seed * (size // len(seed) + 1))[:size]


@pytest.fixture(scope="module")
def table():
    return sngram.weights()


@pytest.fixture(scope="module")
def table_bytes():
    counter = sngram.BigramCounter()
    counter.process(repeated(SOURCE, 1 << 16))
    return counter.to_table_bytes()


@pytest.mark.parametrize("size", SCAN_SIZES)
def test_scan_code(benchmark, table, size):
    data = repeated(SOURCE, size)
    result = benchmark(sngram.scan, table, data)
    assert result.summary.byte_len == size


@pytest.mark.parametrize("size", SCAN_SIZES)
def test_scan_prose(benchmark, table, size):
    data = repeated(PROSE, size)
    result = benchmark(sngram.scan, table, data)
    assert result.summary.byte_len == size


def test_scan_key_bytes(benchmark, table):
    result = sngram.scan(table, repeated(SOURCE, 1 << 18))
    keys = benchmark(result.key_bytes)
    assert len(keys) == len(result.grams) * 8


@pytest.mark.parametrize("pattern", PATTERNS, ids=lambda p: p)
def test_query_plan(benchmark, table, pattern):
    plan = benchmark(sngram.query, table, pattern)
    assert plan.op


def test_query_plan_all(benchmark, table):
    def plan_all():
        return [sngram.query(table, pattern) for pattern in PATTERNS]

    plans = benchmark(plan_all)
    assert len(plans) == len(PATTERNS)


def test_query_plan_tune(benchmark, table):
    plan = sngram.query(table, r"MAX_[A-Z]+_SIZE")
    tuned = benchmark(plan.tune, lambda key: key % 97, total_entries=10_000, stop_df=5_000)
    assert tuned.op


def test_table_from_bytes(benchmark, table_bytes):
    loaded = benchmark(sngram.WeightTable.from_bytes, table_bytes)
    assert loaded.version == 1


def test_table_to_bytes(benchmark):
    counter = sngram.BigramCounter()
    counter.process(repeated(SOURCE, 1 << 16))
    raw = benchmark(counter.to_table_bytes)
    assert raw


@pytest.mark.parametrize("size", COUNT_SIZES)
def test_counter_process(benchmark, size):
    data = repeated(SOURCE, size)

    def process():
        counter = sngram.BigramCounter()
        counter.process(data)
        return counter

    counter = benchmark(process)
    assert counter.bytes_processed == size
