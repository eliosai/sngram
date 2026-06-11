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


def test_scan_and_scan_hashes_agree(table):
    doc = b"pub async fn read_content(hash: Hash) -> Result<Bytes, Error> {}"
    triples = sngram.scan(table, doc)
    keys = np.frombuffer(sngram.scan_hashes(table, doc), dtype=np.uint64)
    assert len(triples) == len(keys) > 0
    for (start, end, h), k in zip(triples, keys):
        assert h == k
        assert 3 <= end - start <= 100
        # the emitted hash equals direct hashing of the gram's bytes
        assert sngram.gram_hash(doc[start:end]) == h


def test_scan_deterministic(table):
    doc = b"fn main() { let x = foo_bar(42); }"
    assert sngram.scan(table, doc) == sngram.scan(table, doc)


def test_scan_short_inputs_empty(table):
    assert sngram.scan(table, b"") == []
    assert sngram.scan(table, b"ab") == []


def test_query_literal_is_and(table):
    plan = sngram.query(table, "MAX_FILE_SIZE")
    assert plan.op == "and"
    assert plan.gram_hashes and len(plan.gram_hashes) == len(plan.grams)
    # query keys must match index keys: every plan gram appears in a scan of
    # any document containing the literal
    doc = b"prefix MAX_FILE_SIZE suffix"
    index_keys = set(np.frombuffer(sngram.scan_hashes(table, doc), dtype=np.uint64))
    for g, h in zip(plan.grams, plan.gram_hashes):
        assert sngram.gram_hash(g) == h
        assert h in index_keys, f"query gram {g!r} missing from index"


def test_query_broad_and_impossible(table):
    assert sngram.query(table, ".*").op == "all"
    assert sngram.query(table, r"[^\s\S]").op == "none"


def test_query_alternation_has_structure(table):
    plan = sngram.query(table, "(a+hello|b+world)")
    assert plan.op == "or"
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
