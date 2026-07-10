from sngram_train.config import (
    CANONICAL_TARGET_BYTES,
    STACK_V2_BUCKET_CAPS,
    hf_token,
    stack_v2_bucket_for,
)


def test_canonical_target_and_area_capacity_are_explicit():
    assert CANONICAL_TARGET_BYTES == 10_000_000_000_000
    assert sum(STACK_V2_BUCKET_CAPS.values()) == 12_000_000_000_000


def test_classifier_routes_known_and_ambiguous_formats():
    assert stack_v2_bucket_for("Python") == "core-programming"
    assert stack_v2_bucket_for("Markdown") == "docs-prose-markup"
    assert stack_v2_bucket_for("Text", extension="json") == "config-build-infra"
    assert stack_v2_bucket_for("Text", extension="csv") == "data-query-schema"
    assert stack_v2_bucket_for("1C Enterprise") == "long-tail"


def test_hf_token_uses_project_environment(monkeypatch):
    monkeypatch.setenv("HF_TOKEN", "token")
    assert hf_token() == "token"
