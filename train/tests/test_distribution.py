from sngram_train.distribution import apportion, mint_schedule, waterfill


def test_area_apportionment_is_exact_and_deterministic():
    weights = {
        "core": 5_200,
        "docs": 2_300,
        "config": 1_500,
        "web": 1_200,
        "data": 1_000,
        "tail": 800,
    }

    shares = apportion(100, weights)

    assert shares == {
        "core": 43,
        "docs": 19,
        "config": 13,
        "web": 10,
        "data": 8,
        "tail": 7,
    }
    assert sum(shares.values()) == 100


def test_waterfill_redistributes_only_after_exhaustion():
    capacities = {"large-a": None, "small": 20, "large-b": 70}

    assert waterfill(120, capacities) == {
        "large-a": 50,
        "small": 20,
        "large-b": 50,
    }


def test_waterfill_assigns_rounding_bytes_by_stable_id():
    assert waterfill(101, {"b": None, "a": None, "c": None}) == {
        "a": 34,
        "b": 34,
        "c": 33,
    }


def test_canonical_schedule_runs_through_ten_tb():
    gb = 10**9
    tb = 10**12

    assert mint_schedule(10 * tb, 1 * tb) == [
        100 * gb,
        500 * gb,
        *range(1 * tb, 10 * tb + 1, 1 * tb),
    ]
