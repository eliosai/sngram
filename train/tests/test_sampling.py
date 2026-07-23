from concurrent.futures import ThreadPoolExecutor

import sngram

from sngram_train.sampling import CountSink, WeightedSlice, count_slices


def test_slices_expand_weighted_documents_exactly():
    batch = count_slices([WeightedSlice(b"ab", 4, 0, 8)])

    assert batch.counter.bytes_processed == 8
    assert batch.counter.count(ord("a"), ord("b")) == 4
    assert batch.effective_bytes == 8
    assert batch.documents == 1


def test_partial_slices_resume_mid_document():
    first = count_slices([WeightedSlice(b"abcd", 4, 0, 10)])
    second = count_slices([WeightedSlice(b"abcd", 4, 10, 6)])

    assert first.counter.bytes_processed == 10
    assert second.counter.bytes_processed == 6
    pairs = first.counter.count(ord("a"), ord("b")) + second.counter.count(
        ord("a"), ord("b")
    )
    assert pairs == 4


def test_count_sink_matches_synchronous_counting():
    slices = tuple(
        WeightedSlice(bytes([65 + i]) * 40, 4, 0, 160) for i in range(8)
    )
    sink = CountSink(sngram.BigramCounter())
    with ThreadPoolExecutor(max_workers=2) as pool:
        sink.pool = pool
        for start in range(0, 8, 2):
            sink.submit(slices[start : start + 2])
        sink.drain()

    reference = count_slices(slices)
    assert sink.counter.snapshot() == reference.counter.snapshot()
    assert sink.counter.bytes_processed == reference.counter.bytes_processed
