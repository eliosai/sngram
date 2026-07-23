import pytest

from sngram_train.distribution import (
    allocate,
    apportion,
    feasible_delta,
    waterlevel,
)


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


def test_waterlevel_redistributes_only_after_exhaustion():
    floors = {"large-a": 0, "small": 0, "large-b": 0}
    caps = {"large-a": None, "small": 20, "large-b": 70}

    goals, shortfall = waterlevel(120, floors, caps)

    assert goals == {"large-a": 50, "small": 20, "large-b": 50}
    assert shortfall == 0


def test_waterlevel_assigns_rounding_bytes_by_stable_id():
    floors = {"b": 0, "a": 0, "c": 0}

    goals, shortfall = waterlevel(101, floors, {key: None for key in floors})

    assert goals == {"a": 34, "b": 34, "c": 33}
    assert shortfall == 0


def test_waterlevel_respects_floors_and_reports_shortfall():
    goals, shortfall = waterlevel(100, {"a": 60, "b": 0}, {"a": 60, "b": 30})

    assert goals == {"a": 60, "b": 30}
    assert shortfall == 10


def test_allocate_lifts_soft_caps_before_reporting_shortfall():
    allocation = allocate(
        100,
        {"a": 0, "b": 0},
        {"a": None, "b": 10},
        {"a": 40, "b": 40},
    )

    assert allocation.goals == {"a": 90, "b": 10}
    assert allocation.shortfall == 0


def test_allocate_prefers_balanced_goals_under_soft_caps():
    allocation = allocate(
        90,
        {"a": 0, "b": 0, "c": 0},
        {"a": None, "b": None, "c": None},
        {"a": 40, "b": 40, "c": 40},
    )

    assert allocation.goals == {"a": 30, "b": 30, "c": 30}
    assert allocation.shortfall == 0


def test_allocate_reports_shortfall_when_hard_supplies_run_out():
    allocation = allocate(
        100,
        {"a": 5, "b": 0},
        {"a": 5, "b": 20},
        {"a": 40, "b": 40},
    )

    assert allocation.goals == {"a": 5, "b": 20}
    assert allocation.shortfall == 75


def test_allocate_keeps_progress_floors_above_shrunken_caps():
    allocation = allocate(
        100,
        {"a": 50, "b": 0},
        {"a": 50, "b": None},
        {"a": 30, "b": 30},
    )

    assert allocation.goals == {"a": 50, "b": 50}
    assert allocation.shortfall == 0


def test_feasible_delta_fits_every_finite_room():
    weights = {"core": 52, "docs": 23, "tail": 25}

    delta = feasible_delta(10_000, weights, {"tail": 100})

    assert apportion(delta, weights)["tail"] <= 100
    assert apportion(delta + 4, weights)["tail"] > 100


def test_waterlevel_rejects_caps_below_floors():
    with pytest.raises(ValueError):
        waterlevel(10, {"a": 5}, {"a": 3})
