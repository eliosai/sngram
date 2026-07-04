"""Production training distribution: Stack v2 metadata + SWH content only."""

from __future__ import annotations

from sngram.train.config import (
    STACK_V2_CONTENT_PREFIX,
    STACK_V2_METADATA_REPO,
    STACK_V2_REQUIRED_COLUMNS,
    STACK_V2_TARGET_BYTES,
    TRAIN_TARGET_BYTES,
    default_families,
    hf_token,
    stack_v2_bucket_for,
    stack_v2_skip_reason,
)
from sngram.train.pipeline import roster_hash


def all_sources():
    return [s for f in default_families() for s in f.sources]


def test_default_distribution_is_stack_v2_swh_only():
    assert TRAIN_TARGET_BYTES == STACK_V2_TARGET_BYTES == 12_000_000_000_000
    assert {s.repo for s in all_sources()} == {STACK_V2_METADATA_REPO}
    assert all(s.format == "swh" for s in all_sources())
    assert all(s.text_field == "blob_id" for s in all_sources())
    assert all(s.content_prefix == STACK_V2_CONTENT_PREFIX for s in all_sources())
    assert all(set(STACK_V2_REQUIRED_COLUMNS) <= set(s.metadata_fields) for s in all_sources())


def test_bucket_caps_match_stack_v2_distribution_doc_exactly():
    buckets = {family.bucket: family.cap_bytes for family in default_families()}
    assert buckets == {
        "core-programming": 5_200_000_000_000,
        "docs-prose-markup": 2_300_000_000_000,
        "config-build-infra": 1_500_000_000_000,
        "web-ui-templates": 1_200_000_000_000,
        "data-query-schema": 1_000_000_000_000,
        "long-tail": 800_000_000_000,
    }
    assert sum(buckets.values()) == STACK_V2_TARGET_BYTES


def test_source_caps_roll_up_to_family_caps():
    for family in default_families():
        assert family.weight == family.cap_bytes / STACK_V2_TARGET_BYTES
        assert len(family.sources) == 1
        assert family.sources[0].family == family.id
        assert family.sources[0].cap_bytes == family.cap_bytes


def test_stack_v2_content_source_is_part_of_roster_identity():
    families = default_families()
    changed_source = families[0].sources[0].__class__(
        **{
            **families[0].sources[0].__dict__,
            "content_prefix": "s3://different/content/",
        }
    )
    changed_family = families[0].__class__(
        **{**families[0].__dict__, "sources": (changed_source,)}
    )
    changed = [changed_family, *families[1:]]

    assert roster_hash(families, STACK_V2_TARGET_BYTES, 1_000_000_000_000) != roster_hash(
        changed, STACK_V2_TARGET_BYTES, 1_000_000_000_000
    )


def test_stack_v2_classifier_routes_major_languages_and_catches_tail():
    assert stack_v2_bucket_for("Python") == "core-programming"
    assert stack_v2_bucket_for("Markdown") == "docs-prose-markup"
    assert stack_v2_bucket_for("YAML") == "config-build-infra"
    assert stack_v2_bucket_for("Vue") == "web-ui-templates"
    assert stack_v2_bucket_for("SQL") == "data-query-schema"
    assert stack_v2_bucket_for("1C Enterprise") == "long-tail"


def test_stack_v2_classifier_uses_path_for_ambiguous_text_files():
    assert stack_v2_bucket_for("Text", path="/docs/install.txt") == "docs-prose-markup"
    assert stack_v2_bucket_for("Text", path="/.github/workflows/test.yml") == "config-build-infra"
    assert stack_v2_bucket_for("Text", extension="csv", path="/data/users.csv") == "data-query-schema"


def test_stack_v2_metadata_filter_skips_before_s3_fetch():
    good = {
        "content_id": "c1",
        "blob_id": "b1",
        "src_encoding": "UTF-8",
        "language": "Python",
        "path": "/src/app.py",
        "is_vendor": False,
        "is_generated": False,
        "length_bytes": 1024,
    }
    assert stack_v2_skip_reason(good) is None

    bad = dict(good, is_vendor=True)
    assert stack_v2_skip_reason(bad) == "vendor"
    bad = dict(good, is_generated=True)
    assert stack_v2_skip_reason(bad) == "generated"
    bad = dict(good, length_bytes=0)
    assert stack_v2_skip_reason(bad) == "empty"
    bad = dict(good, length_bytes=2 * 1024 * 1024 + 1)
    assert stack_v2_skip_reason(bad) == "oversize"
    bad = dict(good, language="Markdown", length_bytes=4 * 1024 * 1024)
    assert stack_v2_skip_reason(bad) is None
    bad = dict(good, language="Markdown", length_bytes=4 * 1024 * 1024 + 1)
    assert stack_v2_skip_reason(bad) == "oversize"


def test_family_ids_unique():
    ids = [f.id for f in default_families()]
    assert len(ids) == len(set(ids))


def test_source_ids_unique():
    ids = [s.id for s in all_sources()]
    assert len(ids) == len(set(ids))


def test_hf_token_uses_environment(monkeypatch):
    monkeypatch.setenv("HF_TOKEN", "hf_test_token")
    assert hf_token() == "hf_test_token"
