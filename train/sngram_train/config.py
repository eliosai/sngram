"""Production Stack v2 corpus contract."""

from __future__ import annotations

import os
from collections.abc import Iterable
from pathlib import Path

_GB = 10**9
_TB = 10**12

CANONICAL_TARGET_BYTES = 6 * _TB
STACK_V2_REVISION = "94d47b4385264b30f228e28a5d63e9b2eee8c2c5"
STACK_V2_CONTENT_PREFIX = "s3://softwareheritage/content/"

# per-area byte budgets, summing to the canonical target
STACK_V2_BUCKET_CAPS = {
    "core-programming": 2_280 * _GB,
    "config-build-infra": 1_182 * _GB,
    "docs-prose-markup": 840 * _GB,
    "web-ui-templates": 822 * _GB,
    "data-query-schema": 696 * _GB,
    "long-tail": 180 * _GB,
}

# fraction of its area budget one format may hold
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

# configs dropped from every area
EXCLUDED_CONFIGS = frozenset({"Jupyter_Notebook"})

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
