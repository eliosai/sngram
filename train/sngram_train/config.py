"""Production Stack v2 corpus contract."""

from __future__ import annotations

import os
from collections.abc import Iterable
from pathlib import Path

GB = 10**9
TB = 10**12
MIB = 1024 * 1024

CANONICAL_TARGET_BYTES = 6 * TB
AVAILABLE_CORPUS_BYTES = 12 * TB
STACK_V2_METADATA_REPO = "bigcode/the-stack-v2-dedup"
STACK_V2_REVISION = "94d47b4385264b30f228e28a5d63e9b2eee8c2c5"
STACK_V2_CONTENT_PREFIX = "s3://softwareheritage/content/"
STACK_V2_MAX_BYTES = 2 * MIB
STACK_V2_DOC_MAX_BYTES = 4 * MIB

STACK_V2_REQUIRED_COLUMNS = (
    "blob_id",
    "content_id",
    "src_encoding",
    "language",
    "path",
    "extension",
    "license_type",
    "is_vendor",
    "is_generated",
    "length_bytes",
)

# area weights double as apportionment ratios and per-format cap basis
# measured Stack supply hard-caps code at 1.94 TB and prose at 0.72 TB, both exhausted
# code lands at 38 to clear 5 TB, docs at 14 to take nearly all prose
# only JSON, HTML, and CSV are elastic, so config, web, data are sized to balance
# those three at ~9 percent each rather than let one spike past the rest
STACK_V2_BUCKET_CAPS = {
    "core-programming": 2_280 * GB,
    "config-build-infra": 1_182 * GB,
    "docs-prose-markup": 840 * GB,
    "web-ui-templates": 822 * GB,
    "data-query-schema": 696 * GB,
    "long-tail": 180 * GB,
}

# per-format cap basis: fraction of the area weight one format may hold
# set well above the balanced level so the area weight governs each share and
# max-min allocation balances formats within an area; a hard stop on domination
AREA_FORMAT_SHARE = {
    "core-programming": 0.30,
    "config-build-infra": 0.48,
    "docs-prose-markup": 0.55,
    "web-ui-templates": 0.72,
    "data-query-schema": 0.78,
    "long-tail": 0.30,
}

GROUP_LABELS = {
    "core-programming": "code",
    "docs-prose-markup": "docs",
    "config-build-infra": "config",
    "web-ui-templates": "web",
    "data-query-schema": "data",
    "long-tail": "other",
}

# configs dropped entirely: bloated base64 JSON, no value for a code byte-pair table
EXCLUDED_CONFIGS = frozenset({"Jupyter_Notebook"})

# extensions dropped entirely: OCR layout dumps that read as markup but carry no code
EXCLUDED_EXTENSIONS = frozenset({"hocr"})

# per-config file-size ceilings that drop generated data blobs from bloat formats
CONFIG_FILE_CAPS = {
    "JSON": 192 * 1024,
    "XML": 192 * 1024,
    "YAML": 160 * 1024,
    "CSV": 384 * 1024,
    "TSV": 384 * 1024,
    "HTML": 768 * 1024,
    "SQL": 512 * 1024,
    "Text": 256 * 1024,
}

CORE_LANGUAGES = {
    "C", "C++", "C#", "Java", "JavaScript", "TypeScript", "Python", "PHP",
    "Go", "Rust", "Ruby", "Swift", "Kotlin", "Scala", "Dart", "Shell",
    "Lua", "R", "Perl", "Objective-C", "Objective-C++", "Fortran",
    "Fortran Free Form", "Pascal", "Visual Basic .NET", "F#", "Haskell",
    "Clojure", "Elixir", "Erlang", "OCaml", "Julia", "MATLAB", "PowerShell",
    "Assembly", "WebAssembly", "Verilog", "SystemVerilog", "VHDL", "Solidity",
    "GLSL", "HLSL", "Cuda", "Zig", "Nim", "Crystal", "D", "Groovy",
    "Common Lisp", "Scheme", "Racket", "Emacs Lisp", "Tcl", "Ada", "COBOL",
    "Prolog", "Smalltalk", "Vala", "Elm", "PureScript", "Haxe", "Hack",
    "Standard ML", "Raku", "Coq",
}

DOC_LANGUAGES = {
    "Text", "Markdown", "reStructuredText", "TeX", "Roff", "Roff Manpage",
    "Org", "Wikitext", "AsciiDoc", "RMarkdown", "Jupyter Notebook", "BibTeX",
    "Textile", "RDoc", "Pod", "Pod 6", "Texinfo",
}

CONFIG_LANGUAGES = {
    "JSON", "JSON with Comments", "JSON5", "YAML", "TOML", "XML", "INI",
    "Dockerfile", "Makefile", "CMake", "Gradle", "Maven POM", "HCL", "Nix",
    "Git Config", "Git Attributes", "Ignore List", "EditorConfig", "Go Module",
    "Go Checksums", "Gemfile.lock", "NPM Config", "Browserslist", "Procfile",
    "Debian Package Control File", "RPM Spec",
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


def stack_config_name(language: str) -> str | None:
    special = {
        "C#": "C-Sharp",
        "F#": "F-Sharp",
        "JSX": None,
        "Visual Basic .NET": "Visual_Basic_.NET",
    }
    if language in special:
        return special[language]
    return language.replace(" ", "_")


def stack_v2_bucket_for(
    language: object, extension: object = None, path: object = None
) -> str:
    """Route one Stack row to its corpus area."""

    lang = _norm(language)
    ext = _norm(extension).lower().lstrip(".")
    normalized_path = "/" + _norm(path).lower().lstrip("/")
    if ext in DATA_EXTENSIONS:
        return "data-query-schema"
    if any(part in normalized_path for part in CONFIG_PATH_PARTS) or ext in CONFIG_EXTENSIONS:
        return "config-build-infra"
    if any(part in normalized_path for part in DOC_PATH_PARTS) or ext in DOC_EXTENSIONS:
        return "docs-prose-markup"
    groups = (
        (CORE_LANGUAGES, "core-programming"),
        (DOC_LANGUAGES, "docs-prose-markup"),
        (CONFIG_LANGUAGES, "config-build-infra"),
        (WEB_LANGUAGES, "web-ui-templates"),
        (DATA_LANGUAGES, "data-query-schema"),
    )
    return next((area for languages, area in groups if lang in languages), "long-tail")


def _norm(value: object) -> str:
    return str(value or "").strip()


def hf_token() -> str | None:
    """Read the Hugging Face token from the environment or train/.env."""

    if token := os.environ.get("HF_TOKEN"):
        return token
    return _env_file_token((Path(".env"), _project_env_path()))


def _project_env_path() -> Path:
    return Path(__file__).resolve().parents[1] / ".env"


def _env_file_token(paths: Iterable[Path]) -> str | None:
    seen: set[Path] = set()
    for path in paths:
        resolved = path.resolve()
        if resolved in seen or not path.exists():
            continue
        seen.add(resolved)
        for line in path.read_text().splitlines():
            value = line.strip().removeprefix("export ").strip()
            if value.startswith("HF_TOKEN="):
                return value.removeprefix("HF_TOKEN=").strip().strip("\"'")
    return None
