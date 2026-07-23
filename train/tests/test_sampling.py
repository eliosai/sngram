from concurrent.futures import ThreadPoolExecutor

import pytest
import sngram

from sngram_train.sampling import CountSink, WeightedDoc, count_documents


def test_documents_expand_by_their_inverse_weight():
    batch = count_documents([WeightedDoc(b"ab", 4), WeightedDoc(b"bc", 1)])

    assert batch.counter.bytes_processed == 10
    assert batch.counter.count(ord("a"), ord("b")) == 4
    assert batch.effective_bytes == 10
    assert batch.documents == 2


def test_empty_or_weightless_documents_are_rejected():
    with pytest.raises(ValueError):
        count_documents([WeightedDoc(b"", 1)])
    with pytest.raises(ValueError):
        count_documents([WeightedDoc(b"ab", 0)])


def test_count_sink_matches_synchronous_counting():
    docs = tuple(WeightedDoc(bytes([65 + i]) * 40, 4) for i in range(8))
    sink = CountSink(sngram.BigramCounter())
    with ThreadPoolExecutor(max_workers=2) as pool:
        sink.pool = pool
        for start in range(0, 8, 2):
            sink.submit(docs[start : start + 2])
        sink.drain()

    reference = count_documents(docs)
    assert sink.counter.snapshot() == reference.counter.snapshot()
    assert sink.counter.bytes_processed == reference.counter.bytes_processed
