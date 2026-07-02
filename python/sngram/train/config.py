"""Training corpus: which Hugging Face datasets feed the counter, and in what
blend.

A *family* is one logical bucket of the mix (e.g. "code", "multilingual"); a
*source* is one streamable unit inside it (a config/language subset, or a whole
repo). Sources shard by file, which is the unit of work, retry, and resume.

The blend targets regex search over **Linux developer filesystems**: measuring a
real disk (text files only, binaries skipped as ripgrep does) shows ~99.9% ASCII
and a code/config/markup-dominant mix. So the corpus is code-heavy, with a small
multilingual slice for UTF-8 coverage. Every repo here is explicitly verified
with `HF_TOKEN` and carries a real streamable text column — no metadata-only
traps, no token-poisoned content (see the roster notes below).

Each family carries a `weight`: its target share of *counted bytes*. The planner
samples sources to hold these shares while data lasts (see pipeline._plan), so
every mint reflects the intended blend rather than raw dataset sizes.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path

GB = 10**9
TB = 10**12
TRAIN_TARGET_BYTES = 15 * TB

# Multilingual coverage: ~12 languages spanning the UTF-8 multibyte space (CJK,
# Cyrillic, Arabic, Greek, Hebrew, Indic/SEA, accented Latin). A real filesystem
# is ~99.9% ASCII, so this is a small coverage slice — enough to give multibyte
# pairs graded weights, not the 90-language web dump the table once trained on.
# English is supplied separately (FinePDFs), so eng_Latn is intentionally absent.
WEB_LANGS = [
    "cmn_Hani", "jpn_Jpan", "kor_Hang", "rus_Cyrl", "arb_Arab", "ell_Grek",
    "heb_Hebr", "hin_Deva", "tha_Thai", "deu_Latn", "fra_Latn", "spa_Latn",
]
CJK_LANGS = {"cmn_Hani", "jpn_Jpan", "kor_Hang"}

REQUIRES_HF_TOKEN = {"bigcode/starcoderdata"}

DOC_PATH_RE = (
    r"(?i:(^|/)(readme|changelog|changes|contributing|contributors|authors|"
    r"license|copying|notice)(\.[^/]*)?$|"
    r"(^|/)(docs?|documentation|examples?|samples?|tutorials?|notebooks?)(/|$)|"
    r"\.(md|markdown|rst|adoc|asciidoc|txt|ipynb)$)"
)

CONFIG_PATH_RE = (
    r"(?i:((^|/)(dockerfile[^/]*|docker-compose[^/]*|makefile|cmakelists\.txt|"
    r"cargo\.lock|go\.sum|package-lock\.json|pnpm-lock\.yaml|yarn\.lock|"
    r"pyproject\.toml|requirements[^/]*\.txt|pom\.xml|build\.gradle|"
    r"gradle\.properties)$|"
    r"(^|/)\.github/workflows/|"
    r"\.(json|ya?ml|toml|xml|ini|cfg|conf|properties|lock|mk|cmake|"
    r"proto|graphql|tf|tfvars|env|editorconfig|gitignore|dockerignore)$))"
)

DOC_OR_CONFIG_PATH_RE = f"(?:{DOC_PATH_RE})|(?:{CONFIG_PATH_RE})"


@dataclass(frozen=True)
class Source:
    """One streamable unit: repo + optional config, with its text field."""

    family: str
    repo: str
    text_field: str
    config: str | None = None
    cap_bytes: int | None = None
    format: str = "parquet"
    # fallback for repos the standard loader can't stream (script datasets):
    # a hf:// parquet glob loaded through the generic parquet builder
    data_files: str | None = None
    path_field: str | None = None
    include_path_regex: str | None = None
    exclude_path_regex: str | None = None
    name: str | None = None

    @property
    def id(self) -> str:
        key = self.name or self.config or self.repo
        return f"{self.family}/{key}"


@dataclass(frozen=True)
class Family:
    """One bucket of the mix: its sources and its target share of counted bytes."""

    id: str
    sources: tuple[Source, ...] = field(default_factory=tuple)
    weight: float = 1.0
    cap_bytes: int | None = None
    bucket: str = ""


def _weight(cap_bytes: int) -> float:
    return cap_bytes / TRAIN_TARGET_BYTES


def _split_caps(total: int, n: int) -> list[int]:
    base, rem = divmod(total, n)
    return [base + (1 if i < rem else 0) for i in range(n)]


def _multilingual_caps() -> dict[str, int]:
    non_cjk = [lang for lang in WEB_LANGS if lang not in CJK_LANGS]
    caps = {lang: 20 * GB for lang in WEB_LANGS if lang in CJK_LANGS}
    caps.update(dict(zip(non_cjk, _split_caps(390 * GB, len(non_cjk)))))
    return caps


def default_families() -> list[Family]:
    """The 15 TB Linux-filesystem training blend.

    Code-dominant (~70%), with technical text/docs/logs, English prose, and a
    small multilingual coverage slice. Weights are target shares of counted
    bytes; the planner holds them while each source's data lasts.
    """

    return [
        # ---- pure code: 10.50 TB / 70% -------------------------------------
        Family(
            id="code-github-2025",
            weight=_weight(2_300 * GB),
            cap_bytes=2_300 * GB,
            bucket="pure-code",
            sources=(
                Source(
                    "code-github-2025", "nick007x/github-code-2025", "content",
                    cap_bytes=2_300 * GB,
                    path_field="file_path",
                    exclude_path_regex=DOC_OR_CONFIG_PATH_RE,
                ),
            ),
        ),
        Family(
            id="code-clippy",
            weight=_weight(6_990 * GB),
            cap_bytes=6_990 * GB,
            bucket="pure-code",
            sources=(
                Source(
                    "code-clippy", "CodedotAI/code_clippy_github", "content",
                    cap_bytes=6_990 * GB,
                    format="json",
                    data_files=(
                        "hf://datasets/CodedotAI/code_clippy_github/"
                        "github-dedup-*.json.gz"
                    ),
                    path_field="path",
                    exclude_path_regex=DOC_OR_CONFIG_PATH_RE,
                ),
            ),
        ),
        Family(
            id="code-stack-v2-high",
            weight=_weight(110 * GB),
            cap_bytes=110 * GB,
            bucket="pure-code",
            sources=(
                Source(
                    "code-stack-v2-high",
                    "M1keR/the-stack-v2-dedup-filtered-500-stars-100-forks-contents",
                    "text",
                    cap_bytes=110 * GB,
                ),
            ),
        ),
        Family(
            id="config-markup",
            weight=_weight(1_100 * GB),
            cap_bytes=1_100 * GB,
            bucket="pure-code",
            sources=(
                Source(
                    "config-markup", "nick007x/github-code-2025", "content",
                    name="github2025-config", cap_bytes=200 * GB,
                    path_field="file_path", include_path_regex=CONFIG_PATH_RE,
                ),
                Source(
                    "config-markup", "CodedotAI/code_clippy_github", "content",
                    name="code-clippy-config", cap_bytes=795 * GB,
                    format="json",
                    data_files=(
                        "hf://datasets/CodedotAI/code_clippy_github/"
                        "github-dedup-*.json.gz"
                    ),
                    path_field="path", include_path_regex=CONFIG_PATH_RE,
                ),
                *tuple(
                    Source(
                        "config-markup", "bigcode/starcoderdata", "content",
                        config=c, cap_bytes=cap,
                        data_files=f"hf://datasets/bigcode/starcoderdata/{c}/*.parquet",
                    )
                    for c, cap in zip(
                        (
                            "markdown", "html", "json", "yaml", "css", "sql",
                            "shell", "makefile", "dockerfile", "cmake",
                            "restructuredtext", "tex", "protocol-buffer",
                            "powershell", "batchfile", "xslt",
                        ),
                        _split_caps(105 * GB, 16),
                    )
                ),
            ),
        ),
        # ---- code/text blend: 3.60 TB / 24% --------------------------------
        Family(
            id="blend-github2025-docs",
            weight=_weight(200 * GB),
            cap_bytes=200 * GB,
            bucket="blend",
            sources=(
                Source(
                    "blend-github2025-docs", "nick007x/github-code-2025", "content",
                    cap_bytes=200 * GB,
                    path_field="file_path", include_path_regex=DOC_PATH_RE,
                ),
            ),
        ),
        Family(
            id="blend-code-clippy-docs",
            weight=_weight(3_000 * GB),
            cap_bytes=3_000 * GB,
            bucket="blend",
            sources=(
                Source(
                    "blend-code-clippy-docs", "CodedotAI/code_clippy_github", "content",
                    cap_bytes=3_000 * GB,
                    format="json",
                    data_files=(
                        "hf://datasets/CodedotAI/code_clippy_github/"
                        "github-dedup-*.json.gz"
                    ),
                    path_field="path", include_path_regex=DOC_PATH_RE,
                ),
            ),
        ),
        Family(
            id="blend-opc",
            weight=_weight(230 * GB),
            cap_bytes=230 * GB,
            bucket="blend",
            sources=(
                Source(
                    "blend-opc", "OpenCoder-LLM/opc-fineweb-code-corpus", "text",
                    cap_bytes=230 * GB,
                ),
            ),
        ),
        Family(
            id="blend-extras",
            weight=_weight(125 * GB),
            cap_bytes=125 * GB,
            bucket="blend",
            sources=tuple(
                Source(
                    "blend-extras", "bigcode/starcoder2data-extras", "content",
                    config=c, cap_bytes=cap,
                )
                for c, cap in zip(
                    ("documentation", "issues", "stackoverflow", "owm"),
                    _split_caps(125 * GB, 4),
                )
            ),
        ),
        Family(
            id="qa-stackoverflow",
            weight=_weight(45 * GB),
            cap_bytes=45 * GB,
            bucket="blend",
            sources=(
                Source(
                    "qa-stackoverflow", "mikex86/stackoverflow-posts", "Body",
                    cap_bytes=45 * GB,
                ),
            ),
        ),
        # ---- English docs: 0.45 TB / 3% ------------------------------------
        Family(
            id="english-finepdfs",
            weight=_weight(450 * GB),
            cap_bytes=450 * GB,
            bucket="english-docs",
            sources=(
                Source(
                    "english-finepdfs", "HuggingFaceFW/finepdfs", "text",
                    config="eng_Latn", cap_bytes=450 * GB,
                ),
            ),
        ),
        # ---- multilingual UTF-8 coverage: 0.45 TB / 3% ---------------------
        Family(
            id="multilingual",
            weight=_weight(450 * GB),
            cap_bytes=450 * GB,
            bucket="multilingual",
            sources=tuple(
                Source(
                    "multilingual", "HuggingFaceFW/fineweb-2", "text",
                    config=lang, cap_bytes=cap,
                )
                for lang, cap in _multilingual_caps().items()
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
