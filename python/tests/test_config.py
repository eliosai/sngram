"""The training corpus roster: every source must be ungated, on-distribution,
and correctly columned, and the mix weights must encode the intended blend.

These are red→green guards on the 10 TB Linux-filesystem corpus: a gated repo,
a metadata-only trap, a stray LLVM-IR config, an untrimmed language list, or a
weight that drops code below half would all break the table, so each is a test.
"""

from __future__ import annotations

from sngram.train import config
from sngram.train.config import WEB_LANGS, default_families

# Repos we verified (via the HF API and a live resolve+count smoke run) are
# ungated, parquet-backed, and carry the named streamable text column. Anything
# outside this set must not appear in the roster. (the-pile-splitted, loghub_2,
# synthetic-syslog, and dockerfiles-linted were dropped: not parquet-backed, so
# they break the bounded-memory direct-parquet read.)
VERIFIED_UNGATED = {
    "nick007x/github-code-2025": "content",
    "codeparrot/github-code": "content",
    "OpenCoder-LLM/opc-fineweb-code-corpus": "text",
    "bigcode/starcoder2data-extras": "content",
    "mikex86/stackoverflow-posts": "Body",
    "substratusai/the-stack-yaml-k8s": "content",
    "HuggingFaceFW/fineweb": "text",
    "wikimedia/wikipedia": "text",
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
    "bigcode/starcoderdata",                # gated + injected <reponame> tokens
}


def all_sources():
    return [s for f in default_families() for s in f.sources]


def test_web_langs_trimmed_to_a_dozen_scripts():
    # the measured filesystem is 99.9% ASCII: multilingual is a small coverage
    # slice, not 90 languages of web text
    assert len(WEB_LANGS) <= 15
    assert "eng_Latn" not in WEB_LANGS  # English comes from fineweb, not here
    # spans the UTF-8 multibyte space: CJK, Cyrillic, Arabic, Greek, Hebrew, Indic
    for needed in ("cmn_Hani", "rus_Cyrl", "arb_Arab", "jpn_Jpan", "hin_Deva"):
        assert needed in WEB_LANGS


def test_no_llvm_ir_configs():
    for s in all_sources():
        assert not (s.config or "").startswith("ir_"), f"LLVM-IR config leaked in: {s.id}"


def test_no_gated_or_metadata_repos():
    for s in all_sources():
        assert s.repo not in FORBIDDEN_REPOS, f"forbidden repo in roster: {s.repo}"


def test_every_repo_is_verified_ungated():
    for s in all_sources():
        assert s.repo in VERIFIED_UNGATED, f"unverified repo: {s.repo}"


def test_text_field_matches_verified_column():
    for s in all_sources():
        assert s.text_field == VERIFIED_UNGATED[s.repo], (
            f"{s.repo}: text_field {s.text_field!r} != verified "
            f"{VERIFIED_UNGATED[s.repo]!r}"
        )


def test_weights_present_and_normalizable():
    fams = default_families()
    assert all(f.weight > 0 for f in fams)
    total = sum(f.weight for f in fams)
    assert total > 0
    norm = {f.id: f.weight / total for f in fams}
    assert abs(sum(norm.values()) - 1.0) < 1e-9


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
