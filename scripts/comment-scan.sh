#!/usr/bin/env bash
# Fail when any comment runs past one line
set -euo pipefail

cd "$(dirname "$0")/.."

python3 - <<'PY'
import pathlib
import sys

def kind(line):
    text = line.strip()
    if text.startswith("//!"):
        return "//!"
    if text.startswith("///"):
        return "///"
    if text.startswith("//"):
        return "//"
    return None

bad = []
paths = [path for path in pathlib.Path("crates").rglob("*.rs") if "target" not in path.parts]
for path in sorted(paths):
    run = []
    for number, line in enumerate(path.read_text().splitlines(), start=1):
        marker = kind(line)
        if run and (marker != run[0][1]):
            if len(run) > 1:
                bad.append((path, run[0][0], run[-1][0]))
            run = []
        if marker:
            run.append((number, marker))
        else:
            run = []
    if len(run) > 1:
        bad.append((path, run[0][0], run[-1][0]))

for path, start, end in bad:
    print(f"{path}:{start}: comment runs {end - start + 1} lines, every comment is one line")

sys.exit(1 if bad else 0)
PY
