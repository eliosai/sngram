"""The nine-family production blend: its sources and their byte ceilings."""

from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass

from . import hub
from .corpus import Corpus, CorpusIdentity, Quota, Reading, Shard
from .errors import ConfigurationError

GB = 10**9
TB = 10**12

NAME = "blend"

TARGET_BYTES = 15 * TB

# ~12 languages spanning the UTF-8 multibyte space
WEB_LANGS = (
    "cmn_Hani", "jpn_Jpan", "kor_Hang", "rus_Cyrl", "arb_Arab", "ell_Grek",
    "heb_Hebr", "hin_Deva", "tha_Thai", "deu_Latn", "fra_Latn", "spa_Latn",
)

CJK_LANGS = frozenset({"cmn_Hani", "jpn_Jpan", "kor_Hang"})

MARKUP_LANGS = (
    "markdown", "html", "json", "yaml", "css", "sql", "shell", "makefile",
    "dockerfile", "cmake", "restructuredtext", "tex", "protocol-buffer",
    "powershell", "batchfile", "xslt",
)

EXTRAS_CONFIGS = ("documentation", "issues", "stackoverflow", "owm")

_STACK_V2_REPO = "M1keR/the-stack-v2-dedup-filtered-500-stars-100-forks-contents"

_FINEWEB_REPO = "HuggingFaceFW/fineweb-2"

_CONTENT = Reading("parquet-column", "content")
_TEXT = Reading("parquet-column", "text")


@dataclass(frozen=True)
class Source:
    """One streamable unit: a repo path prefix, its text column, its ceiling"""

    family: str
    repo: str
    prefix: str
    reading: Reading
    cap_bytes: int
    config: str | None = None

    @property
    def id(self) -> str:
        if self.config:
            return f"{self.family}/{self.config}"
        return f"{self.family}/{self.repo}"

    @property
    def suffix(self) -> str:
        return ".json.gz" if self.reading.layout == "json-lines" else ".parquet"


@dataclass(frozen=True)
class Family:
    """One bucket of the mix: its sources and its ceiling"""

    id: str
    cap_bytes: int
    sources: tuple[Source, ...]


def default_families() -> tuple[Family, ...]:
    """The 15 TB Linux-filesystem blend that minted the production table."""

    return (
        _single("code-clippy", "CodedotAI/code_clippy_github", "github-dedup-",
                Reading("json-lines", "content"), 8_300 * GB),
        _single("code-github-2025", "nick007x/github-code-2025", "",
                _CONTENT, 2_300 * GB),
        _single("code-stack-v2-high", _STACK_V2_REPO, "data/", _TEXT, 200 * GB),
        _configs("config-markup", "bigcode/starcoderdata", MARKUP_LANGS,
                 _CONTENT, 450 * GB),
        _single("blend-opc", "OpenCoder-LLM/opc-fineweb-code-corpus", "data/",
                _TEXT, 265 * GB),
        _configs("blend-extras", "bigcode/starcoder2data-extras", EXTRAS_CONFIGS,
                 _CONTENT, 2_690 * GB),
        _single("qa-stackoverflow", "mikex86/stackoverflow-posts",
                "stackoverflow-posts-", Reading("parquet-column", "Body"), 45 * GB),
        _single("english-finepdfs", "HuggingFaceFW/finepdfs",
                "data/eng_Latn/train/", _TEXT, 300 * GB, config="eng_Latn"),
        _multilingual(),
    )


def _single(
    family: str,
    repo: str,
    prefix: str,
    reading: Reading,
    cap: int,
    config: str | None = None,
) -> Family:
    return Family(family, cap, (Source(family, repo, prefix, reading, cap, config),))


def _configs(
    family: str, repo: str, configs: tuple[str, ...], reading: Reading, total: int
) -> Family:
    caps = _split_caps(total, len(configs))
    sources = tuple(
        Source(family, repo, f"{config}/", reading, cap, config)
        for config, cap in zip(configs, caps)
    )
    return Family(family, total, sources)


def _multilingual() -> Family:
    sources = tuple(
        Source("multilingual", _FINEWEB_REPO, f"data/{lang}/train/", _TEXT, cap, lang)
        for lang, cap in _multilingual_caps().items()
    )
    return Family("multilingual", 450 * GB, sources)


def _multilingual_caps() -> dict[str, int]:
    """20 GB per CJK language, the rest split evenly over the others."""

    others = [lang for lang in WEB_LANGS if lang not in CJK_LANGS]
    caps = {lang: 20 * GB for lang in WEB_LANGS if lang in CJK_LANGS}
    caps.update(dict(zip(others, _split_caps(390 * GB, len(others)))))
    return caps


def _split_caps(total: int, count: int) -> list[int]:
    base, remainder = divmod(total, count)
    return [base + (1 if index < remainder else 0) for index in range(count)]


def resolve(token: str | None, note=None) -> Corpus:
    """Pin every source repo and list the shards the blend will stream."""

    families = default_families()
    listings: dict[str, hub.Listing] = {}
    shards: list[Shard] = []
    for family in families:
        for source in family.sources:
            listed = _listing(source.repo, token, note, listings)
            shards.extend(_source_shards(source, listed))
    identity = CorpusIdentity(NAME, fingerprint(families, listings))
    return Corpus(identity, tuple(shards), quota(families))


def _listing(
    repo: str, token: str | None, note, listings: dict[str, hub.Listing]
) -> hub.Listing:
    if repo not in listings:
        listings[repo] = hub.listing(repo, token, note)
    return listings[repo]


def _source_shards(source: Source, listed: hub.Listing) -> list[Shard]:
    matched = listed.under(source.prefix, source.suffix)
    if not matched:
        raise ConfigurationError(
            f"{source.id} matched no {source.suffix} files under {source.prefix!r}"
        )
    return [
        Shard(listed.hub_path(path), size, source.reading, source.family, source.id)
        for path, size in matched
    ]


def quota(families: tuple[Family, ...]) -> Quota:
    """The byte ceilings this roster enforces."""

    return Quota(
        {family.id: family.cap_bytes for family in families},
        {
            source.id: source.cap_bytes
            for family in families
            for source in family.sources
        },
    )


def fingerprint(
    families: tuple[Family, ...], listings: dict[str, hub.Listing]
) -> str:
    """Stable identity of the roster and the revisions it is pinned to."""

    payload = {
        "target": TARGET_BYTES,
        "revisions": {repo: listed.revision for repo, listed in sorted(listings.items())},
        "families": [_family_payload(family) for family in families],
    }
    encoded = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def _family_payload(family: Family) -> dict:
    return {
        "id": family.id,
        "cap_bytes": family.cap_bytes,
        "sources": [
            {
                "id": source.id,
                "repo": source.repo,
                "prefix": source.prefix,
                "config": source.config,
                "layout": source.reading.layout,
                "field": source.reading.field,
                "cap_bytes": source.cap_bytes,
            }
            for source in family.sources
        ],
    }
