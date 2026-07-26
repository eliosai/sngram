import pytest

from sngram_train import planner
from sngram_train.corpus import Corpus, CorpusIdentity, Quota, Reading, Shard, Tallies

READING = Reading("parquet-column", "text")


def corpus_of(layout: dict[str, dict[str, int]], caps: dict[str, int]) -> Corpus:
    """One corpus from {family: {source: shard count}} and per-bucket ceilings."""

    shards = [
        Shard(f"{source}/{index}", 1, READING, family, source)
        for family, sources in layout.items()
        for source, count in sources.items()
        for index in range(count)
    ]
    families = {family: caps[family] for family in layout}
    sources = {source: caps[source] for group in layout.values() for source in group}
    identity = CorpusIdentity("blend", "fingerprint")
    return Corpus(identity, tuple(shards), Quota(families, sources))


def drain(schedule, corpus: Corpus, tallies: Tallies, per_shard: int) -> list[str]:
    """Dispatch until the schedule stops, counting `per_shard` bytes each time."""

    taken = []
    while (index := schedule.next(tallies)) is not None:
        shard = corpus.shards[index]
        taken.append(shard.family)
        tallies.add(shard, per_shard)
        tallies.finish(shard)
    return taken


def test_ordered_walks_every_shard_once_and_resumes_at_its_cursor():
    schedule = planner.Ordered(3, None)
    tallies = Tallies()

    assert [schedule.next(tallies) for _ in range(4)] == [0, 1, 2, None]
    assert schedule.state() == {"cursor": 3}
    assert planner.Ordered(3, {"cursor": 1}).next(tallies) == 1


def test_a_corpus_without_a_quota_gets_the_ordered_schedule():
    corpus = Corpus(CorpusIdentity("stack-v3", "rev"), ())

    assert isinstance(planner.schedule(corpus, None, Tallies()), planner.Ordered)


def test_the_blend_holds_its_target_shares_while_data_lasts():
    corpus = corpus_of(
        {"big": {"big/a": 200}, "small": {"small/a": 200}},
        {"big": 750, "big/a": 750, "small": 250, "small/a": 250},
    )
    tallies = Tallies()
    schedule = planner.schedule(corpus, None, tallies)

    taken = drain(schedule, corpus, tallies, per_shard=5)

    assert tallies.family_bytes["big"] == 750
    assert tallies.family_bytes["small"] == 250
    share = taken.count("big") / len(taken)
    assert 0.70 < share < 0.80, "the 75/25 target holds across the whole run"


def test_a_family_stops_being_picked_once_it_reaches_its_ceiling():
    corpus = corpus_of(
        {"capped": {"capped/a": 50}, "open": {"open/a": 50}},
        {"capped": 10, "capped/a": 10, "open": 40, "open/a": 40},
    )
    tallies = Tallies()

    drain(planner.schedule(corpus, None, tallies), corpus, tallies, per_shard=1)

    assert tallies.family_bytes["capped"] == 10
    assert tallies.family_bytes["open"] == 40


def test_a_source_ceiling_caps_one_subset_without_stopping_its_family():
    corpus = corpus_of(
        {"fam": {"fam/small": 50, "fam/large": 50}},
        {"fam": 60, "fam/small": 10, "fam/large": 50},
    )
    tallies = Tallies()

    drain(planner.schedule(corpus, None, tallies), corpus, tallies, per_shard=1)

    assert tallies.source_bytes["fam/small"] == 10
    assert tallies.source_bytes["fam/large"] == 50
    assert tallies.family_bytes["fam"] == 60


def test_a_family_that_runs_dry_leaves_the_rest_to_carry_the_blend():
    corpus = corpus_of(
        {"thin": {"thin/a": 3}, "deep": {"deep/a": 100}},
        {"thin": 500, "thin/a": 500, "deep": 100, "deep/a": 100},
    )
    tallies = Tallies()

    taken = drain(planner.schedule(corpus, None, tallies), corpus, tallies, per_shard=1)

    assert taken.count("thin") == 3, "an exhausted source drops out"
    assert tallies.family_bytes["deep"] == 100


def test_sources_inside_one_family_take_turns():
    corpus = corpus_of(
        {"fam": {"fam/a": 4, "fam/b": 4}},
        {"fam": 8, "fam/a": 8, "fam/b": 8},
    )
    tallies = Tallies()
    schedule = planner.schedule(corpus, None, tallies)

    order = []
    while (index := schedule.next(tallies)) is not None:
        shard = corpus.shards[index]
        order.append(shard.source)
        tallies.add(shard, 1)
        tallies.finish(shard)

    assert order == ["fam/a", "fam/b"] * 4


def test_a_resumed_blend_keeps_balancing_against_the_whole_run():
    corpus = corpus_of(
        {"big": {"big/a": 200}, "small": {"small/a": 200}},
        {"big": 150, "big/a": 150, "small": 50, "small/a": 50},
    )
    tallies = Tallies()
    first = planner.schedule(corpus, None, tallies)
    for _ in range(40):
        shard = corpus.shards[first.next(tallies)]
        tallies.add(shard, 1)
        tallies.finish(shard)
    saved = first.state()

    resumed = planner.schedule(corpus, saved, tallies)
    drain(resumed, corpus, tallies, per_shard=1)

    assert tallies.family_bytes["big"] == 150
    assert tallies.family_bytes["small"] == 50
    assert resumed.state()["cursors"]["big/a"] == 150


def test_in_flight_shards_are_priced_so_a_burst_cannot_skew_the_blend():
    corpus = corpus_of(
        {"a": {"a/s": 50}, "b": {"b/s": 50}},
        {"a": 50, "a/s": 50, "b": 50, "b/s": 50},
    )
    tallies = Tallies()
    schedule = planner.schedule(corpus, None, tallies)

    # dispatch ten shards without counting a single byte back
    families = [corpus.shards[schedule.next(tallies)].family for _ in range(10)]

    assert families.count("a") == families.count("b") == 5


def test_estimated_bytes_prices_in_flight_work_at_the_bucket_mean():
    estimate = planner.estimated_bytes(
        counted={"a": 200}, completed={"a": 2}, dispatched={"a": 4, "b": 1}
    )

    assert estimate["a"] == 200 + 100 * 2
    assert estimate["b"] == 100, "an unfinished bucket is priced at the global mean"


def test_pick_family_takes_the_largest_deficit():
    weights = {"a": 0.75, "b": 0.25}

    assert planner.pick_family(["a", "b"], weights, {}) == "a"
    assert planner.pick_family(["a", "b"], weights, {"a": 100.0}) == "b"
    assert planner.pick_family(["b"], weights, {"a": 1.0, "b": 0.0}) == "b"


def test_a_picked_family_always_has_a_dispatchable_source():
    corpus = corpus_of({"fam": {"fam/a": 1}}, {"fam": 1, "fam/a": 1})
    tallies = Tallies()
    schedule = planner.schedule(corpus, None, tallies)

    with pytest.raises(RuntimeError, match="no dispatchable source"):
        schedule._take("fam", {"fam/a": 99.0})
