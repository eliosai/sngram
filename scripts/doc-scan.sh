#!/usr/bin/env bash
# Fail when a Rust example is neither compiled as a doc test nor marked as prose
set -euo pipefail

cd "$(dirname "$0")/.."

python3 - <<'PY'
import pathlib
import re
import sys

FENCE = re.compile(r"^\s*(?:///|//!)?\s*```\s*([A-Za-z0-9_,\s-]*)$")
RUST = {"rust", "rs"}
SKIPPED = {"ignore", "no_run", "compile_fail"}


def sources():
    for path in pathlib.Path("crates").rglob("*.rs"):
        if "target" not in path.parts:
            yield path


def markdown():
    yield pathlib.Path("README.md")
    for root in ("docs", "crates"):
        for path in pathlib.Path(root).rglob("*.md"):
            if "target" not in path.parts:
                yield path


def doctested():
    """Markdown a crate pulls into its doc comment, so rustdoc compiles its examples."""
    found = set()
    for path in sources():
        for include in re.findall(r'include_str!\("([^"]+)"\)', path.read_text()):
            target = (path.parent / include).resolve()
            if target.suffix == ".md":
                found.add(target)
    return found


def attributes(info):
    return {word.strip() for word in info.replace(",", " ").split() if word.strip()}


bad = []
compiled = doctested()
for path in markdown():
    is_doctest = path.resolve() in compiled
    for number, line in enumerate(path.read_text().splitlines(), start=1):
        match = FENCE.match(line)
        if not match:
            continue
        info = attributes(match.group(1))
        if not info & RUST:
            continue
        if not is_doctest:
            bad.append(f"{path}:{number}: a rust block outside a doc test, mark it text")
        elif info & SKIPPED:
            bad.append(f"{path}:{number}: a rust block the doc tests skip")

for path in sources():
    for number, line in enumerate(path.read_text().splitlines(), start=1):
        match = FENCE.match(line)
        if match and attributes(match.group(1)) & SKIPPED:
            bad.append(f"{path}:{number}: a rust block the doc tests skip")

for line in bad:
    print(line)
sys.exit(1 if bad else 0)
PY
