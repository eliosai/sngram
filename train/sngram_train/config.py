"""Training corpus configuration.

The production roster is Stack v2 dedup metadata plus Software Heritage content:
metadata rows come from Hugging Face, and file bytes are fetched from SWH S3 by
`blob_id`. The public surface stays small: `default_families()` describes the
counted-byte distribution, and helpers classify/filter Stack v2 metadata before
any S3 content fetch.
"""

from __future__ import annotations

import os
from collections.abc import Iterable
from dataclasses import dataclass, field
from pathlib import Path

GB = 10**9
TB = 10**12
MIB = 1024 * 1024

STACK_V2_TARGET_BYTES = 12 * TB
TRAIN_TARGET_BYTES = STACK_V2_TARGET_BYTES

STACK_V2_METADATA_REPO = "bigcode/the-stack-v2-dedup"
STACK_V2_CONTENT_PREFIX = "s3://softwareheritage/content/"
STACK_V2_REQUIRED_COLUMNS = (
    "blob_id",
    "content_id",
    "src_encoding",
    "language",
    "path",
    "extension",
    "is_vendor",
    "is_generated",
    "length_bytes",
)

REQUIRES_HF_TOKEN = {STACK_V2_METADATA_REPO}

STACK_V2_BUCKET_CAPS = {
    "core-programming": 5_200 * GB,
    "docs-prose-markup": 2_300 * GB,
    "config-build-infra": 1_500 * GB,
    "web-ui-templates": 1_200 * GB,
    "data-query-schema": 1_000 * GB,
    "long-tail": 800 * GB,
}

STACK_V2_MAX_BYTES = 2 * MIB
STACK_V2_DOC_MAX_BYTES = 4 * MIB
STACK_V2_MIN_BYTES = int(os.environ.get("SNG_STACK_MIN_BYTES", 16 * 1024))

CORE_LANGUAGES = {
    "C", "C++", "C#", "Java", "JavaScript", "TypeScript", "Python", "PHP",
    "Go", "Rust", "Ruby", "Swift", "Kotlin", "Scala", "Dart", "Shell",
    "Lua", "R", "Perl", "Objective-C", "Objective-C++", "Fortran",
    "Fortran Free Form", "Pascal", "Visual Basic .NET", "F#", "Haskell",
    "Clojure", "Elixir", "Erlang", "OCaml", "Julia", "MATLAB", "PowerShell",
}

DOC_LANGUAGES = {
    "Text", "Markdown", "reStructuredText", "TeX", "Roff", "Roff Manpage",
    "Org", "Wikitext", "AsciiDoc", "RMarkdown", "Jupyter Notebook", "BibTeX",
    "Textile", "RDoc", "Pod", "Pod 6", "Texinfo",
}
STACK_V2_SOURCE_MAX_SHARE = float(os.environ.get("SNG_STACK_SOURCE_MAX_SHARE", "0.06"))

CONFIG_LANGUAGES = {
    "JSON", "JSON with Comments", "JSON5", "YAML", "TOML", "XML", "INI",
    "Dockerfile", "Makefile", "CMake", "Gradle", "Maven POM", "HCL", "Nix",
    "Git Config", "Git Attributes", "Ignore List", "EditorConfig",
    "Go Module", "Go Checksums", "Gemfile.lock", "NPM Config", "Browserslist",
    "Procfile", "Debian Package Control File", "RPM Spec",
}

WEB_LANGUAGES = {
    "HTML", "HTML+ERB", "HTML+EEX", "HTML+ECR", "HTML+PHP", "HTML+Razor",
    "CSS", "SCSS", "Sass", "Less", "Vue", "Svelte", "Blade", "EJS",
    "Java Server Pages", "Groovy Server Pages", "Twig", "Liquid", "Handlebars",
    "Pug", "Haml", "Astro", "TSX", "JSX", "Mustache", "Smarty", "Slim",
}

DATA_LANGUAGES = {
    "SQL", "PLSQL", "PLpgSQL", "TSQL", "CSV", "TSV", "GraphQL",
    "Protocol Buffer", "Protocol Buffer Text Format", "Thrift", "ASN.1",
    "Avro IDL", "Turtle", "Web Ontology Language", "SPARQL", "JSONLD",
    "HiveQL", "RAML", "API Blueprint",
}

DOC_PATH_PARTS = ("/docs/", "/doc/", "/documentation/", "/examples/", "/notebooks/")
CONFIG_PATH_PARTS = ("/.github/workflows/",)
DOC_EXTENSIONS = {"md", "markdown", "rst", "adoc", "asciidoc", "txt", "tex", "org"}
CONFIG_EXTENSIONS = {
    "json", "yaml", "yml", "toml", "xml", "ini", "cfg", "conf", "properties",
    "lock", "mk", "cmake", "env", "editorconfig", "gitignore", "dockerignore",
}
DATA_EXTENSIONS = {"csv", "tsv", "sql", "graphql", "proto", "ttl", "rdf", "owl"}


@dataclass(frozen=True)
class Source:
    """One streamable unit.

    For ordinary parquet/json fixtures this is still repo + text field. For the
    production Stack v2 roster, `format="swh"` means rows are metadata and the
    actual content lives under `content_prefix + blob_id`.
    """

    family: str
    repo: str
    text_field: str
    config: str | None = None
    cap_bytes: int | None = None
    format: str = "parquet"
    data_files: str | None = None
    content_prefix: str | None = None
    metadata_fields: tuple[str, ...] = ()
    bucket: str | None = None

    @property
    def id(self) -> str:
        return f"{self.family}/{self.config}" if self.config else f"{self.family}/{self.repo}"


@dataclass(frozen=True)
class Family:
    """One bucket of the mix: its sources and target share of counted bytes."""

    id: str
    sources: tuple[Source, ...] = field(default_factory=tuple)
    weight: float = 1.0
    cap_bytes: int | None = None
    bucket: str = ""


def _weight(cap_bytes: int) -> float:
    return cap_bytes / TRAIN_TARGET_BYTES


def _stack_config_name(language: str) -> str | None:
    special = {
        "C#": "C-Sharp",
        "F#": "F-Sharp",
        "JSX": None,
        "Visual Basic .NET": "Visual_Basic_.NET",
    }
    if language in special:
        return special[language]
    return language.replace(" ", "_")


def _stack_source(fid: str, bucket: str, config: str, cap: int) -> Source:
    return Source(
        fid,
        STACK_V2_METADATA_REPO,
        "blob_id",
        config=config,
        cap_bytes=cap,
        format="swh",
        content_prefix=STACK_V2_CONTENT_PREFIX,
        metadata_fields=STACK_V2_REQUIRED_COLUMNS,
        bucket=bucket,
    )


def _stack_sources(fid: str, bucket: str, cap: int) -> tuple[Source, ...]:
    languages = {
        "core-programming": CORE_LANGUAGES,
        "docs-prose-markup": DOC_LANGUAGES,
        "config-build-infra": CONFIG_LANGUAGES | {"Text"},
        "web-ui-templates": WEB_LANGUAGES,
        "data-query-schema": DATA_LANGUAGES | {"Text"},
        "long-tail": (),
    }[bucket]
    if bucket == "long-tail":
        return (_stack_source(fid, bucket, "default", cap),)
    configs = sorted({c for lang in languages if (c := _stack_config_name(lang))})
    return tuple(_stack_source(fid, bucket, config, cap) for config in configs)


def default_families() -> list[Family]:
    """The 12 TB Stack v2 / Software Heritage production blend."""

    return [
        Family(
            id=f"stack-{bucket}",
            weight=_weight(cap),
            cap_bytes=cap,
            bucket=bucket,
            sources=_stack_sources(f"stack-{bucket}", bucket, cap),
        )
        for bucket, cap in STACK_V2_BUCKET_CAPS.items()
    ]


def stack_v2_source_cap(source: Source, source_count: int = 1) -> int | None:
    if not _stack_v2_cap_applies(source, source_count):
        return source.cap_bytes
    share_cap = int((source.cap_bytes or 0) * STACK_V2_SOURCE_MAX_SHARE)
    return min(source.cap_bytes or share_cap, share_cap)


def _stack_v2_cap_applies(source: Source, source_count: int) -> bool:
    if source_count <= 1:
        return False
    if source.format != "swh":
        return False
    if source.bucket not in STACK_V2_BUCKET_CAPS:
        return False
    return source.cap_bytes is not None


def stack_v2_bucket_source_caps(family: Family) -> dict[str, int | None]:
    source_count = len(family.sources)
    return {
        source.id: stack_v2_source_cap(source, source_count)
        for source in family.sources
    }


def stack_v2_bucket_source_capacity(family: Family) -> int | None:
    caps = stack_v2_bucket_source_caps(family).values()
    if any(cap is None for cap in caps):
        return None
    return sum(int(cap or 0) for cap in caps)


def _norm(value: object) -> str:
    return str(value or "").strip()


def stack_v2_bucket_for(
    language: str | None, extension: str | None = None, path: str | None = None
) -> str:
    """Bucket a Stack v2 metadata row using language first, with path/extension
    overrides for ambiguous text-like rows."""

    lang = _norm(language)
    ext = _norm(extension).lower().lstrip(".")
    normalized_path = "/" + _norm(path).lower().lstrip("/")

    if ext in DATA_EXTENSIONS:
        return "data-query-schema"
    if any(part in normalized_path for part in CONFIG_PATH_PARTS) or ext in CONFIG_EXTENSIONS:
        return "config-build-infra"
    if any(part in normalized_path for part in DOC_PATH_PARTS) or ext in DOC_EXTENSIONS:
        return "docs-prose-markup"
    if lang in CORE_LANGUAGES:
        return "core-programming"
    if lang in DOC_LANGUAGES:
        return "docs-prose-markup"
    if lang in CONFIG_LANGUAGES:
        return "config-build-infra"
    if lang in WEB_LANGUAGES:
        return "web-ui-templates"
    if lang in DATA_LANGUAGES:
        return "data-query-schema"
    return "long-tail"


def stack_v2_skip_reason(row: dict[str, object]) -> str | None:
    """Return why a metadata row should be skipped before S3 fetch, or None."""

    if row.get("is_vendor") is True:
        return "vendor"
    if row.get("is_generated") is True:
        return "generated"
    try:
        length = int(row.get("length_bytes") or 0)
    except (TypeError, ValueError):
        return "bad_length"
    if length <= 0:
        return "empty"
    if length < STACK_V2_MIN_BYTES:
        return "small"
    bucket = stack_v2_bucket_for(
        _norm(row.get("language")),
        _norm(row.get("extension")),
        _norm(row.get("path")),
    )
    limit = STACK_V2_DOC_MAX_BYTES if bucket == "docs-prose-markup" else STACK_V2_MAX_BYTES
    if length > limit:
        return "oversize"
    for field in ("blob_id", "content_id", "src_encoding", "language"):
        if not row.get(field):
            return f"missing_{field}"
    return None


def _project_env_path() -> Path:
    return Path(__file__).resolve().parents[1] / ".env"


def _env_file_token(paths: Iterable[Path]) -> str | None:
    seen: set[Path] = set()
    for env in paths:
        resolved = env.resolve()
        if resolved in seen or not env.exists():
            continue
        seen.add(resolved)
        for line in env.read_text().splitlines():
            line = line.strip()
            if line.startswith("export "):
                line = line.removeprefix("export ").strip()
            if line.startswith("HF_TOKEN="):
                token = line.removeprefix("HF_TOKEN=").strip()
                return token.strip("\"'")
    return None


def hf_token() -> str | None:
    """HF_TOKEN from the environment or the training project's .env file."""

    if tok := os.environ.get("HF_TOKEN"):
        return tok
    return _env_file_token((Path(".env"), _project_env_path()))
