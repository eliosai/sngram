"""Training corpus: which Hugging Face datasets feed the counter, and in what
blend.

A *family* is one logical bucket of the mix (e.g. "code", "multilingual"); a
*source* is one streamable unit inside it (a config/language subset, or a whole
repo). Sources shard by file, which is the unit of work, retry, and resume.

The blend targets regex search over **Linux developer filesystems**: measuring a
real disk (text files only, binaries skipped as ripgrep does) shows ~99.9% ASCII
and a code/config/markup-dominant mix. So the corpus is code-heavy, with a small
multilingual slice for UTF-8 coverage. Every repo here is ungated and carries a
real streamable text column — no gated sets, no metadata-only traps, no
token-poisoned content (see the roster notes below).

Each family carries a `weight`: its target share of *counted bytes*. The planner
samples sources to hold these shares while data lasts (see pipeline._plan), so
every mint reflects the intended blend rather than raw dataset sizes.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path

# Multilingual coverage: ~12 languages spanning the UTF-8 multibyte space (CJK,
# Cyrillic, Arabic, Greek, Hebrew, Indic/SEA, accented Latin). A real filesystem
# is ~99.9% ASCII, so this is a small coverage slice — enough to give multibyte
# pairs graded weights, not the 90-language web dump the table once trained on.
# English is supplied separately (fineweb), so eng_Latn is intentionally absent.
WEB_LANGS = [
    "cmn_Hani", "jpn_Jpan", "kor_Hang", "rus_Cyrl", "arb_Arab", "ell_Grek",
    "heb_Hebr", "hin_Deva", "tha_Thai", "deu_Latn", "fra_Latn", "spa_Latn",
]

# starcoder2data-extras: docs and code-adjacent prose (NOT the LLVM-IR configs,
# which are machine-generated and out-of-distribution for filesystem search).
EXTRAS_CONFIGS = [
    "documentation", "issues", "stackoverflow", "wikipedia", "arxiv", "owm",
    "lhq", "kaggle",
]


@dataclass(frozen=True)
class Source:
    """One streamable unit: repo + optional config, with its text field."""

    family: str
    repo: str
    text_field: str
    config: str | None = None
    # fallback for repos the standard loader can't stream (script datasets):
    # a hf:// parquet glob loaded through the generic parquet builder
    data_files: str | None = None

    @property
    def id(self) -> str:
        return f"{self.family}/{self.config}" if self.config else f"{self.family}/{self.repo}"


@dataclass(frozen=True)
class Family:
    """One bucket of the mix: its sources and its target share of counted bytes."""

    id: str
    sources: tuple[Source, ...] = field(default_factory=tuple)
    weight: float = 1.0


def default_families() -> list[Family]:
    """The ~10 TB Linux-filesystem training blend.

    Code-dominant (>=50%), with technical text/docs/logs, English prose, and a
    small multilingual coverage slice. Weights are target shares of counted
    bytes; the planner holds them while each source's data lasts.
    """

    return [
        # ---- code (>=50%): real repo content as it sits on disk -------------
        Family(
            id="code-github-2025",
            weight=0.30,
            sources=(Source("code-github-2025", "nick007x/github-code-2025", "content"),),
        ),
        Family(
            id="code-github",
            weight=0.15,
            sources=(
                # script dataset: stream its parquet files directly. Its text
                # lives in `content` (not `code`, despite the loader's docs).
                Source(
                    "code-github", "codeparrot/github-code", "content",
                    data_files="hf://datasets/codeparrot/github-code/data/*.parquet",
                ),
            ),
        ),
        Family(
            id="code-opc",
            weight=0.05,
            sources=(Source("code-opc", "OpenCoder-LLM/opc-fineweb-code-corpus", "text"),),
        ),
        # ---- technical text / docs / config (~17%) -------------------------
        Family(
            id="docs-extras",
            weight=0.10,
            sources=tuple(
                Source("docs-extras", "bigcode/starcoder2data-extras", "content", config=c)
                for c in EXTRAS_CONFIGS
            ),
        ),
        Family(
            id="qa-stackoverflow",
            weight=0.05,
            sources=(Source("qa-stackoverflow", "mikex86/stackoverflow-posts", "Body"),),
        ),
        Family(
            id="config",
            weight=0.02,
            sources=(Source("config", "substratusai/the-stack-yaml-k8s", "content"),),
        ),
        # ---- English prose (~23%) ------------------------------------------
        Family(
            id="english-fineweb",
            weight=0.14,
            sources=(
                Source("english-fineweb", "HuggingFaceFW/fineweb", "text", config="sample-350BT"),
            ),
        ),
        Family(
            id="english-wikipedia",
            weight=0.09,
            sources=(
                Source(
                    "english-wikipedia", "wikimedia/wikipedia", "text", config="20231101.en"
                ),
            ),
        ),
        # ---- multilingual UTF-8 coverage (~10%) ----------------------------
        Family(
            id="multilingual",
            weight=0.10,
            sources=tuple(
                Source("multilingual", "HuggingFaceFW/fineweb-2", "text", config=lang)
                for lang in WEB_LANGS
            ),
        ),
    ]


def hf_token() -> str | None:
    """HF_TOKEN from the environment or a local .env file."""
    if tok := os.environ.get("HF_TOKEN"):
        return tok
    env = Path(".env")
    if env.exists():
        for line in env.read_text().splitlines():
            if line.startswith("HF_TOKEN="):
                return line.removeprefix("HF_TOKEN=").strip()
    return None
