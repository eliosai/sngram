"""The training corpus roster: every source must be accessible, on-distribution,
and correctly columned, and the mix caps must encode the intended blend.

These are red->green guards on the 15 TB Linux-filesystem corpus: a forbidden
repo, a metadata-only trap, a stray LLVM-IR config, an untrimmed language list,
or a cap that lets multilingual dominate would all break the table.
"""

from __future__ import annotations

from sngram.train import config
from sngram.train.config import (
    CJK_LANGS,
    REQUIRES_HF_TOKEN,
    TRAIN_TARGET_BYTES,
    WEB_LANGS,
    default_families,
    hf_token,
)

# Reviewed repos in the capped 15 TB roster. Live HF tests verify access/schema
# with the user's token; this static guard prevents accidental unreviewed sources.
VERIFIED_ACCESSIBLE = {
    "nick007x/github-code-2025": "content",
    "CodedotAI/code_clippy_github": "content",
    "M1keR/the-stack-v2-dedup-filtered-500-stars-100-forks-contents": "text",
    "bigcode/starcoderdata": "content",
    "OpenCoder-LLM/opc-fineweb-code-corpus": "text",
    "bigcode/starcoder2data-extras": "content",
    "mikex86/stackoverflow-posts": "Body",
    "HuggingFaceFW/finepdfs": "text",
    "HuggingFaceFW/fineweb-2": "text",
}

# Gated, metadata-only, or token-poisoned repos that must never be streamed.
FORBIDDEN_REPOS = {
    "nvidia/Nemotron-Pretraining-Code-v2",  # gated=manual
    "nvidia/Nemotron-Pretraining-Code-v3",  # metadata only
    "bigcode/the-stack-v2",                 # metadata only (SWHIDs)
    "bigcode/the-stack-v2-dedup",           # metadata only
    "bigcode/the-stack-dedup",              # gated=auto
    "bigcode/the-stack",                    # gated
}


def all_sources():
    return [s for f in default_families() for s in f.sources]


def test_web_langs_trimmed_to_a_dozen_scripts():
    # the measured filesystem is 99.9% ASCII: multilingual is a small coverage
    # slice, not 90 languages of web text
    assert len(WEB_LANGS) <= 15
    assert "eng_Latn" not in WEB_LANGS  # English comes from FinePDFs, not here
    # spans the UTF-8 multibyte space: CJK, Cyrillic, Arabic, Greek, Hebrew, Indic
    for needed in ("cmn_Hani", "rus_Cyrl", "arb_Arab", "jpn_Jpan", "hin_Deva"):
        assert needed in WEB_LANGS


def test_no_llvm_ir_configs():
    for s in all_sources():
        assert not (s.config or "").startswith("ir_"), f"LLVM-IR config leaked in: {s.id}"


def test_no_forbidden_or_metadata_repos():
    for s in all_sources():
        assert s.repo not in FORBIDDEN_REPOS, f"forbidden repo in roster: {s.repo}"


def test_every_repo_is_verified_accessible():
    for s in all_sources():
        assert s.repo in VERIFIED_ACCESSIBLE, f"unverified repo: {s.repo}"


def test_text_field_matches_verified_column():
    for s in all_sources():
        assert s.text_field == VERIFIED_ACCESSIBLE[s.repo], (
            f"{s.repo}: text_field {s.text_field!r} != verified "
            f"{VERIFIED_ACCESSIBLE[s.repo]!r}"
        )


def test_token_required_sources_are_declared():
    repos = {s.repo for s in all_sources()}
    assert "bigcode/starcoderdata" in REQUIRES_HF_TOKEN
    assert REQUIRES_HF_TOKEN <= repos


def test_weights_present_and_normalizable():
    fams = default_families()
    assert all(f.weight > 0 for f in fams)
    total = sum(f.weight for f in fams)
    assert total > 0
    norm = {f.id: f.weight / total for f in fams}
    assert abs(sum(norm.values()) - 1.0) < 1e-9


def test_distribution_caps_are_hard_targets():
    fams = default_families()
    caps = {f.id: f.cap_bytes for f in fams}
    assert all(cap is not None and cap > 0 for cap in caps.values())
    assert sum(caps.values()) == TRAIN_TARGET_BYTES == 15_000_000_000_000
    assert caps["multilingual"] == 450_000_000_000
    assert caps["multilingual"] / TRAIN_TARGET_BYTES == 0.03


def test_source_and_cjk_caps_roll_up_exactly():
    for family in default_families():
        source_caps = [s.cap_bytes for s in family.sources]
        assert all(cap is not None and cap > 0 for cap in source_caps), family.id
        assert sum(source_caps) == family.cap_bytes, family.id

    ml = next(f for f in default_families() if f.id == "multilingual")
    cjk = [s for s in ml.sources if s.config in CJK_LANGS]
    assert len(cjk) == len(CJK_LANGS)
    assert sum(s.cap_bytes for s in cjk) == 60_000_000_000
    assert all(s.cap_bytes <= 20_000_000_000 for s in cjk)


def test_bucket_caps_match_distribution_doc_exactly():
    buckets: dict[str, int] = {}
    for family in default_families():
        buckets[family.bucket] = buckets.get(family.bucket, 0) + family.cap_bytes
    assert buckets == {
        "pure-code": 11_250_000_000_000,
        "blend": 3_000_000_000_000,
        "english-docs": 300_000_000_000,
        "multilingual": 450_000_000_000,
    }


def test_code_is_at_least_half_the_blend():
    fams = default_families()
    total = sum(f.weight for f in fams)
    code = sum(f.weight for f in fams if f.id.startswith("code-")) / total
    assert code >= 0.50, f"code share {code:.2%} < 50%"


def test_multilingual_is_a_minor_slice():
    # the filesystem is ASCII-dominant; multilingual must not dominate
    fams = default_families()
    total = sum(f.weight for f in fams)
    ml = sum(f.weight for f in fams if f.id == "multilingual") / total
    assert 0 < ml <= 0.15, f"multilingual share {ml:.2%} out of range"


def test_family_ids_unique():
    ids = [f.id for f in default_families()]
    assert len(ids) == len(set(ids))


def test_source_family_matches_owning_family():
    for f in default_families():
        for s in f.sources:
            assert s.family == f.id, f"{s.id}: source.family {s.family!r} != {f.id!r}"


def test_hf_token_uses_environment(monkeypatch):
    monkeypatch.setenv("HF_TOKEN", "hf_test_token")
    assert hf_token() == "hf_test_token"
