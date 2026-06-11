"""Counting differential tests: the Arrow path must match a naive Python count."""

import random
from collections import Counter as PyCounter

import pyarrow as pa
import pytest

import sngram


def naive_counts(rows: list[bytes]) -> PyCounter:
    c: PyCounter = PyCounter()
    for row in rows:
        for a, b in zip(row, row[1:]):
            c[(a, b)] += 1
    return c


def assert_matches(counter: sngram.BigramCounter, rows: list[bytes]) -> None:
    expected = naive_counts(rows)
    for (a, b), n in expected.items():
        assert counter.count(a, b) == n, f"pair ({a},{b})"
    assert counter.pairs_processed == sum(expected.values())
    assert counter.bytes_processed == sum(len(r) for r in rows)


def random_rows(seed: int, n: int) -> list[bytes]:
    rng = random.Random(seed)
    return [
        bytes(rng.randrange(256) for _ in range(rng.randrange(0, 200))) for _ in range(n)
    ]


@pytest.mark.parametrize("arrow_type", [pa.string(), pa.large_string()])
def test_count_arrow_matches_naive_strings(arrow_type):
    rng = random.Random(7)
    rows = [
        "".join(chr(rng.randrange(32, 127)) for _ in range(rng.randrange(0, 120)))
        for _ in range(500)
    ]
    raw = [r.encode() for r in rows]
    tbl = pa.table({"content": pa.array(rows, type=arrow_type)})

    tally = sngram.LocalTally()
    counted = tally.count_arrow(tbl)
    assert counted == sum(len(r) for r in raw)

    counter = sngram.BigramCounter()
    counter.merge(tally)
    assert_matches(counter, raw)


@pytest.mark.parametrize("arrow_type", [pa.binary(), pa.large_binary()])
def test_count_arrow_matches_naive_binary(arrow_type):
    rows = random_rows(11, 300)
    tbl = pa.table({"content": pa.array(rows, type=arrow_type)})
    tally = sngram.LocalTally()
    tally.count_arrow(tbl)
    counter = sngram.BigramCounter()
    counter.merge(tally)
    assert_matches(counter, rows)


def test_nulls_are_skipped():
    tbl = pa.table({"c": pa.array(["ab", None, "cd"], type=pa.large_string())})
    tally = sngram.LocalTally()
    assert tally.count_arrow(tbl) == 4
    counter = sngram.BigramCounter()
    counter.merge(tally)
    assert counter.count(ord("a"), ord("b")) == 1
    assert counter.count(ord("b"), ord("c")) == 0, "no pair may straddle rows/nulls"


def test_chunked_input_equals_contiguous():
    rows = ["alpha", "beta", "gamma", "delta"] * 100
    contiguous = pa.table({"c": pa.array(rows, type=pa.large_string())})
    chunked = pa.table(
        {"c": pa.chunked_array([pa.array(rows[:150]), pa.array(rows[150:])])}
    )
    t1, t2 = sngram.LocalTally(), sngram.LocalTally()
    assert t1.count_arrow(contiguous) == t2.count_arrow(chunked)
    c1, c2 = sngram.BigramCounter(), sngram.BigramCounter()
    c1.merge(t1)
    c2.merge(t2)
    for a, b in {(ord(x), ord(y)) for r in rows for x, y in zip(r, r[1:])}:
        assert c1.count(a, b) == c2.count(a, b)


def test_record_batch_and_reader_inputs():
    rows = ["hello world", "foo bar"]
    batch = pa.record_batch({"c": pa.array(rows, type=pa.large_string())})
    t1 = sngram.LocalTally()
    assert t1.count_arrow(batch) == sum(len(r) for r in rows)

    reader = pa.RecordBatchReader.from_batches(batch.schema, [batch, batch])
    t2 = sngram.LocalTally()
    assert t2.count_arrow(reader) == 2 * sum(len(r) for r in rows)


def test_non_arrow_input_raises():
    tally = sngram.LocalTally()
    with pytest.raises(TypeError):
        tally.count_arrow([1, 2, 3])


def test_merge_accumulates_and_count_bytes():
    counter = sngram.BigramCounter()
    for _ in range(3):
        t = sngram.LocalTally()
        t.count(b"ab")
        counter.merge(t)
    assert counter.count(ord("a"), ord("b")) == 3
    assert counter.files_processed == 0
    counter.add_files(2)
    assert counter.files_processed == 2


def test_minted_table_obeys_inverse_frequency():
    counter = sngram.BigramCounter()
    for _ in range(100):
        counter.process(b"the quick brown fox")
    counter.process(b"zqzq")
    table = sngram.WeightTable.from_bytes(counter.to_table_bytes())
    assert table.weight(ord("z"), ord("q")) > table.weight(ord("t"), ord("h"))
