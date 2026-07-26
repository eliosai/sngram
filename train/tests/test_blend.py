from types import SimpleNamespace

import pytest

from sngram_train import blend, hub
from sngram_train.errors import ConfigurationError

FAMILY_CAPS = {
    "code-clippy": 8_300 * blend.GB,
    "code-github-2025": 2_300 * blend.GB,
    "code-stack-v2-high": 200 * blend.GB,
    "config-markup": 450 * blend.GB,
    "blend-opc": 265 * blend.GB,
    "blend-extras": 2_690 * blend.GB,
    "qa-stackoverflow": 45 * blend.GB,
    "english-finepdfs": 300 * blend.GB,
    "multilingual": 450 * blend.GB,
}


def families():
    return {family.id: family for family in blend.default_families()}


def test_the_roster_carries_the_nine_families_that_minted_production():
    assert {family.id for family in blend.default_families()} == set(FAMILY_CAPS)


def test_family_ceilings_sum_to_the_fifteen_terabyte_target():
    assert {f.id: f.cap_bytes for f in blend.default_families()} == FAMILY_CAPS
    assert sum(FAMILY_CAPS.values()) == blend.TARGET_BYTES


def test_every_family_ceiling_is_the_sum_of_its_source_ceilings():
    for family in blend.default_families():
        total = sum(source.cap_bytes for source in family.sources)
        assert total == family.cap_bytes, family.id


def test_sources_carry_their_repo_layout_and_text_field():
    listed = {
        source.id: source
        for family in blend.default_families()
        for source in family.sources
    }

    clippy = listed["code-clippy/CodedotAI/code_clippy_github"]
    assert clippy.reading == blend.Reading("json-lines", "content")
    assert clippy.suffix == ".json.gz"
    assert clippy.prefix == "github-dedup-"

    posts = listed["qa-stackoverflow/mikex86/stackoverflow-posts"]
    assert posts.reading.field == "Body"

    assert listed["english-finepdfs/eng_Latn"].prefix == "data/eng_Latn/train/"
    assert listed["multilingual/cmn_Hani"].reading.field == "text"
    assert listed["config-markup/markdown"].reading.field == "content"


def test_the_multilingual_slice_gives_cjk_a_fixed_share():
    sources = {s.config: s for s in families()["multilingual"].sources}

    assert set(sources) == set(blend.WEB_LANGS)
    assert all(sources[lang].cap_bytes == 20 * blend.GB for lang in blend.CJK_LANGS)
    others = [s.cap_bytes for lang, s in sources.items() if lang not in blend.CJK_LANGS]
    assert sum(others) == 390 * blend.GB
    assert max(others) - min(others) <= 1, "the rest split evenly"


def test_markup_and_extras_split_their_family_ceiling_evenly():
    markup = [s.cap_bytes for s in families()["config-markup"].sources]
    extras = [s.cap_bytes for s in families()["blend-extras"].sources]

    assert len(markup) == len(blend.MARKUP_LANGS) == 16
    assert max(markup) - min(markup) <= 1
    assert len(extras) == len(blend.EXTRAS_CONFIGS) == 4
    assert extras == [672_500_000_000] * 4


def test_the_quota_covers_every_family_and_source():
    roster = blend.default_families()
    quota = blend.quota(roster)

    assert quota.families == FAMILY_CAPS
    assert len(quota.sources) == sum(len(family.sources) for family in roster)


def test_the_fingerprint_moves_with_the_roster_and_the_pinned_revisions():
    roster = blend.default_families()
    pinned = {"org/a": hub.Listing("org/a", "one", ())}
    drifted = {"org/a": hub.Listing("org/a", "two", ())}

    assert blend.fingerprint(roster, pinned) == blend.fingerprint(roster, pinned)
    assert blend.fingerprint(roster, pinned) != blend.fingerprint(roster, drifted)
    assert blend.fingerprint(roster[:2], pinned) != blend.fingerprint(roster, pinned)


SHARDS_PER_SOURCE = 2


class RosterApi:
    """Serves exactly the files each roster source asks its repo for"""

    empty = False

    def __init__(self, token=None):
        pass

    def dataset_info(self, repo, files_metadata=False, timeout=None):
        names = [] if RosterApi.empty else _roster_files(repo)
        return SimpleNamespace(
            sha=f"sha-{repo.replace('/', '-')}",
            siblings=[
                SimpleNamespace(rfilename=name, size=size) for name, size in names
            ],
        )


def _roster_files(repo: str) -> list[tuple[str, int]]:
    return [
        (f"{source.prefix}part-{index}{source.suffix}", 10 + index)
        for family in blend.default_families()
        for source in family.sources
        if source.repo == repo
        for index in range(SHARDS_PER_SOURCE)
    ]


def test_resolve_stamps_every_shard_with_its_repo_revision_and_bucket(monkeypatch):
    RosterApi.empty = False
    monkeypatch.setattr("huggingface_hub.HfApi", RosterApi)
    roster = blend.default_families()

    corpus = blend.resolve(token=None)

    sources = [source for family in roster for source in family.sources]
    assert corpus.identity.name == "blend"
    assert len(corpus.shards) == len(sources) * SHARDS_PER_SOURCE
    assert corpus.quota == blend.quota(roster)
    by_source = {shard.source for shard in corpus.shards}
    assert by_source == {source.id for source in sources}
    clippy = next(s for s in corpus.shards if s.family == "code-clippy")
    assert clippy.path == (
        "datasets/CodedotAI/code_clippy_github@sha-CodedotAI-code_clippy_github"
        "/github-dedup-part-0.json.gz"
    )
    assert clippy.reading.layout == "json-lines"


def test_resolve_lists_each_repo_once(monkeypatch):
    RosterApi.empty = False
    calls: list[str] = []
    original = RosterApi.dataset_info

    def counted(self, repo, files_metadata=False, timeout=None):
        calls.append(repo)
        return original(self, repo, files_metadata, timeout)

    monkeypatch.setattr(RosterApi, "dataset_info", counted)
    monkeypatch.setattr("huggingface_hub.HfApi", RosterApi)

    blend.resolve(token=None)

    assert len(calls) == len(set(calls)) == 9, "one pinned listing per source repo"


def test_resolve_fails_when_a_source_matches_no_files(monkeypatch):
    RosterApi.empty = True
    monkeypatch.setattr("huggingface_hub.HfApi", RosterApi)

    with pytest.raises(ConfigurationError, match="matched no"):
        blend.resolve(token=None)
