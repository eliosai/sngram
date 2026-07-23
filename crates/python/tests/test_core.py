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
    result = sngram.scan(table, b"fn main() { let x = foo_bar(42); }")
    assert len(result.grams) > 0


def test_scan_grams_and_key_bytes_agree(table):
    doc = b"pub async fn read_content(hash: Hash) -> Result<Bytes, Error> {}"
    result = sngram.scan(table, doc)
    keys = np.frombuffer(result.key_bytes(), dtype=np.uint64)
    assert len(result.grams) == len(keys) > 0
    for (start, end, h), k in zip(result.grams, keys):
        assert h == k
        # sentinel-bracketed grams may span fewer than 3 content bytes
        assert 1 <= end - start <= 16
        assert end <= len(doc)


def test_scan_summary_reports_document_facts(table):
    doc = b"fn main() {\n\n    foo_bar(42);\n}"
    result = sngram.scan(table, doc)
    summary = result.summary
    assert summary.byte_len == len(doc)
    assert summary.line_count == 4
    assert summary.empty_line_count == 1
    assert summary.longest_line_len == len(b"    foo_bar(42);")
    assert summary.gram_count == len(result.grams)
    assert doc.startswith(summary.prefix)
    assert doc.endswith(summary.suffix)


def test_scan_deterministic(table):
    doc = b"fn main() { let x = foo_bar(42); }"
    assert sngram.scan(table, doc).grams == sngram.scan(table, doc).grams


def test_scan_short_inputs(table):
    assert sngram.scan(table, b"").grams == []
    # short inputs still index through sentinel-bracketed grams
    short = sngram.scan(table, b"ab")
    assert all(end <= 2 for (_, end, _) in short.grams)


def test_query_literal_is_and(table):
    plan = sngram.query(table, "MAX_FILE_SIZE")
    assert plan.op == "and"
    assert plan.grams
    # every logical gram needle must appear in a scan of a matching document
    doc = b"prefix MAX_FILE_SIZE suffix"
    index_keys = set(np.frombuffer(sngram.scan(table, doc).key_bytes(), dtype=np.uint64))
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


def test_query_needs_evaluate_against_summaries(table):
    plan = sngram.query(table, "MAX_FILE_SIZE")
    assert plan.needs
    matching = sngram.scan(table, b"prefix MAX_FILE_SIZE suffix").summary
    assert all(need.satisfied_by(matching) for need in plan.needs)
    tiny = sngram.scan(table, b"x").summary
    assert not all(need.satisfied_by(tiny) for need in plan.needs)


def test_query_tune_orders_and_thins(table):
    plan = sngram.query(table, "MAX_FILE_SIZE")
    tuned = plan.tune(lambda key: 1, total_entries=100, stop_df=50)
    assert tuned.op == plan.op
    assert 0 < tuned.gram_count <= plan.gram_count
    with pytest.raises(TypeError):
        plan.tune(lambda key: "not a count", total_entries=100, stop_df=50)


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
    assert t1.to_bytes() == data
    with pytest.raises(ValueError):
        sngram.WeightTable.from_bytes(b"junk")


def test_weight_table_from_fn_and_matrix():
    table = sngram.WeightTable.from_weight_fn(lambda c1, c2: c1 * 256 + c2)
    assert table.weight(1, 2) == 258
    matrix = np.frombuffer(table.matrix(), dtype=np.uint32).reshape(256, 256)
    assert matrix[1, 2] == 258
    assert matrix[255, 255] == 255 * 256 + 255
    with pytest.raises(ZeroDivisionError):
        sngram.WeightTable.from_weight_fn(lambda c1, c2: c1 // 0)


def test_weight_table_provenance(table):
    assert table.provenance is None or isinstance(table.provenance, str)
    stamped = table.with_provenance("test mint")
    assert stamped.provenance == "test mint"
    assert stamped.weight(ord("f"), ord("n")) == table.weight(ord("f"), ord("n"))


def test_scan_rejects_binary_input(table):
    with pytest.raises(RuntimeError, match="binary"):
        sngram.scan(table, b"\x00" * 64)
