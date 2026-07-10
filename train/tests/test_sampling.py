import sngram

from sngram_train.sampling import count_weighted, sample_weight


def test_large_files_are_always_selected():
    assert sample_weight("content-a", 16 * 1024) == 1
    assert sample_weight("content-b", 2 * 1024 * 1024) == 1


def test_small_file_sampling_is_deterministic_and_unbiased():
    decisions = [sample_weight(f"content-{index}", 4096) for index in range(4096)]

    assert decisions == [sample_weight(f"content-{index}", 4096) for index in range(4096)]
    selected = [weight for weight in decisions if weight is not None]
    assert set(selected) == {4}
    assert 900 <= len(selected) <= 1150


def test_weighted_count_scales_pairs_and_bytes():
    batch = count_weighted([(b"ab", 4), (b"bc", 1)], limit=9)

    assert isinstance(batch.counter, sngram.BigramCounter)
    assert batch.counter.bytes_processed == 9
    assert batch.counter.count(ord("a"), ord("b")) == 4
    assert batch.effective_bytes == 9
    assert batch.documents == 2


def test_weighted_count_trims_to_exact_effective_limit():
    batch = count_weighted([(b"abcd", 4)], limit=10)

    assert batch.effective_bytes == 10
    assert batch.counter.bytes_processed == 10
    assert batch.counter.count(ord("a"), ord("b")) == 3
