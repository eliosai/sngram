import pytest

from sngram_train.corpus import (
    Corpus,
    CorpusIdentity,
    CorpusName,
    Quota,
    Reading,
    Shard,
    Sifted,
    Tallies,
    resolve,
)
from sngram_train.errors import ConfigurationError

READING = Reading("parquet-column", "text")


def shard(source: str, size: int) -> Shard:
    return Shard(f"{source}/{size}", size, READING, source, source)


def test_reading_rejects_an_unknown_layout():
    with pytest.raises(ConfigurationError, match="layout"):
        Reading("csv", "text")


def test_reading_rejects_a_missing_text_field():
    with pytest.raises(ConfigurationError, match="text field"):
        Reading("parquet-column", "")


def test_identity_stamps_a_short_fingerprint_and_binds_the_full_one():
    identity = CorpusIdentity("blend", "0123456789abcdef")

    assert identity.stamp() == "blend@0123456789ab"
    assert identity.binding() == "blend@0123456789abcdef"


def test_take_keeps_the_first_shards_of_every_source():
    corpus = Corpus(
        CorpusIdentity("blend", "f"),
        (shard("a", 1), shard("a", 2), shard("b", 4), shard("b", 8)),
    )

    assert corpus.wire_bytes() == 15
    assert corpus.take(1).wire_bytes() == 5, "one shard per source, not one overall"
    assert [s.source for s in corpus.take(1).shards] == ["a", "b"]


def test_quota_reports_the_tighter_of_the_family_and_source_ceilings():
    quota = Quota({"fam": 100}, {"src": 40})
    tallies = Tallies()
    target = Shard("p", 1, READING, "fam", "src")

    assert quota.remaining(target, tallies) == 40
    tallies.add(target, 35)
    assert quota.remaining(target, tallies) == 5
    tallies.add(target, 50)
    assert quota.remaining(target, tallies) == 0, "a ceiling never goes negative"


def test_tallies_track_bytes_and_completions_per_bucket():
    tallies = Tallies()
    target = Shard("p", 1, READING, "fam", "src")

    tallies.add(target, 7)
    tallies.finish(target)

    assert tallies.family_bytes == {"fam": 7}
    assert tallies.source_bytes == {"src": 7}
    assert tallies.family_done == {"fam": 1}
    assert tallies.source_done == {"src": 1}


def sifted_of(*values: str) -> Sifted:
    import pyarrow as pa

    column = pa.array(list(values), type=pa.string())
    return Sifted(pa.record_batch([column], names=["content"]), len(values), len(values))


def test_head_returns_the_batch_untouched_when_it_fits():
    batch = sifted_of("abc", "de")

    assert batch.head(5) is batch
    assert batch.head(99) is batch


def test_head_trims_to_the_longest_row_prefix_inside_the_budget():
    batch = sifted_of("abc", "de", "fghij")

    trimmed = batch.head(6)

    assert trimmed.content.column(0).to_pylist() == ["abc", "de"]
    assert trimmed.files == 2
    assert trimmed.head(0).content.num_rows == 0


def test_resolve_dispatches_to_the_named_corpus(monkeypatch):
    seen = []
    monkeypatch.setattr(
        "sngram_train.blend.resolve", lambda token, note: seen.append("blend")
    )
    monkeypatch.setattr(
        "sngram_train.stack.resolve", lambda token, note: seen.append("stack")
    )

    resolve(CorpusName.BLEND, None, None)
    resolve(CorpusName.STACK_V3, None, None)

    assert seen == ["blend", "stack"]
