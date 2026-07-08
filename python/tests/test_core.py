"""Bindings correctness: scan/query/hash consistency against simple oracles."""

import numpy as np
import pytest

import sngram


CORPUS = (
    b"fn main() { let x = foo_bar(42); }",
    b"pub async fn read_content(hash: Hash) -> Result<Bytes, Error> {}",
    b"the quick brown fox jumps over the lazy dog",
    b"MAX_FILE_SIZE prefix MAX_FILE_SIZE suffix",
    b"SELECT grams FROM content_ngrams WHERE grams @> ARRAY[1,2,3];",
)


@pytest.fixture(scope="module")
def table():
    # train a small real table so the hull is non-trivial
    counter = sngram.BigramCounter()
    for doc in CORPUS * 3:
        counter.process(doc)
    return sngram.WeightTable.from_bytes(counter.to_table_bytes())


def test_trained_table_loads(table):
    assert table.version == 1
    assert table.weight(ord("f"), ord("n")) > 0


def test_embedded_production_table_loads():
    table = sngram.weights()
    assert table.fingerprint != 0
    keys = sngram.scan_hashes(table, b"fn main() { let x = foo_bar(42); }")
    assert len(keys) > 0


def test_scan_and_scan_hashes_agree(table):
    doc = b"pub async fn read_content(hash: Hash) -> Result<Bytes, Error> {}"
    triples = sngram.scan(table, doc)
    keys = np.frombuffer(sngram.scan_hashes(table, doc), dtype=np.uint64)
    assert len(triples) == len(keys) > 0
    for (start, end, h), k in zip(triples, keys):
        assert h == k
        # sentinel-bracketed grams may span fewer than 3 content bytes
        assert 1 <= end - start <= 16
        assert end <= len(doc)


def test_scan_deterministic(table):
    doc = b"fn main() { let x = foo_bar(42); }"
    assert sngram.scan(table, doc) == sngram.scan(table, doc)


def test_scan_short_inputs(table):
    assert sngram.scan(table, b"") == []
    # short inputs still index through sentinel-bracketed grams
    short = sngram.scan(table, b"ab")
    assert all(end <= 2 for (_, end, _) in short)


def test_query_literal_is_and(table):
    plan = sngram.query(table, "MAX_FILE_SIZE")
    assert plan.op == "and"
    assert plan.grams
    # Query keys must match index keys: every logical gram needle appears in a
    # scan of any document containing the literal.
    doc = b"prefix MAX_FILE_SIZE suffix"
    index_keys = set(np.frombuffer(sngram.scan_hashes(table, doc), dtype=np.uint64))
    for alternatives in plan.grams:
        assert alternatives
        assert any(key in index_keys for key in alternatives)


def test_query_broad_and_impossible(table):
    assert sngram.query(table, ".*").op == "all"
    assert sngram.query(table, r"[^\s\S]").op == "none"


def test_query_alternation_has_structure(table):
    plan = sngram.query(table, "(a+hello|b+world)")
    # root scan-needs wrap the alternation in an "and"
    assert plan.op == "and"
    assert any(child.op == "or" for child in plan.children)
    assert "QueryPlan(" in repr(plan)


def test_query_invalid_pattern_raises(table):
    with pytest.raises(ValueError):
        sngram.query(table, "(unclosed")


def test_weight_table_round_trip(tmp_path, table):
    c = sngram.BigramCounter()
    c.process(b"the quick brown fox")
    data = c.to_table_bytes()
    path = tmp_path / "t.bin"
    path.write_bytes(data)
    t1 = sngram.WeightTable.from_bytes(data)
    t2 = sngram.WeightTable.from_path(path)
    assert t1.weight(ord("t"), ord("h")) == t2.weight(ord("t"), ord("h"))
    with pytest.raises(ValueError):
        sngram.WeightTable.from_bytes(b"junk")
