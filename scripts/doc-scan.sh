#!/usr/bin/env bash
# Fail when any Rust example in the docs is marked so the doc tests skip it
set -euo pipefail

cd "$(dirname "$0")/.."

if grep -rnE '^\s*(///|//!)?\s*```(rust,)?(ignore|no_run|compile_fail)' README.md docs crates --include='*.md' --include='*.rs'; then
    echo "documentation contains a Rust example the doc tests skip" >&2
    exit 1
fi
