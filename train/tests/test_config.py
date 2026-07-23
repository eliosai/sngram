from sngram_train.config import (
    CANONICAL_TARGET_BYTES,
    STACK_V2_BUCKET_CAPS,
    hf_token,
)


def test_canonical_target_and_area_capacity_are_explicit():
    assert CANONICAL_TARGET_BYTES == 6_000_000_000_000
    assert sum(STACK_V2_BUCKET_CAPS.values()) == 6_000_000_000_000
    code = STACK_V2_BUCKET_CAPS["core-programming"]
    assert code / sum(STACK_V2_BUCKET_CAPS.values()) == 0.38
    docs = STACK_V2_BUCKET_CAPS["docs-prose-markup"]
    assert docs / sum(STACK_V2_BUCKET_CAPS.values()) == 0.14


def test_hf_token_uses_project_environment(monkeypatch):
    monkeypatch.setenv("HF_TOKEN", "token")
    assert hf_token() == "token"
