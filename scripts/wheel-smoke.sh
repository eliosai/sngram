#!/usr/bin/env bash
# Install the freshly built wheel into a throwaway environment and exercise the public surface
set -euo pipefail

cd "$(dirname "$0")/.."

venv=$(mktemp -d)
uv venv -q "$venv"
uv pip install -q --python "$venv/bin/python" target/wheel-smoke/sngram-*.whl
"$venv/bin/python" - <<'PY'
import sngram

table = sngram.weights()
result = sngram.scan(table, b"fn main() {}")
assert result.grams and result.summary.byte_len == 12
assert sngram.query(table, "main").op == "and"
assert sngram.is_binary(b"\x00") and not sngram.is_binary(b"fn main() {}")
print("wheel ok:", sngram.__version__)
PY
rm -rf "$venv"
