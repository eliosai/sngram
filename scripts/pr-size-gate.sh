#!/usr/bin/env bash
# Fails the pr when its counted insertions exceed the review budget
# Counted = added lines outside vendored and generated paths; deletions are free
# Budgets: 1000 default, 3000 with the `mechanical` label, none with `size-exempt`
set -euo pipefail

BASE_SHA="${BASE_SHA:?}"
HEAD_SHA="${HEAD_SHA:?}"
LABELS="${LABELS:-}"

EXCLUDES=(":(exclude)Cargo.lock" ":(exclude)**/uv.lock" ":(exclude).agents/skills/**" ":(exclude)CHANGELOG.md" ":(exclude)**/*.tsv" ":(exclude)crates/lib/data/**")

added=$(git diff --numstat "$BASE_SHA" "$HEAD_SHA" -- . "${EXCLUDES[@]}" |
    awk '$1 != "-" { sum += $1 } END { print sum + 0 }')

budget=1000
if [[ " $LABELS " == *" mechanical "* ]]; then
    budget=3000
fi

echo "counted insertions: $added (budget: $budget, labels: ${LABELS:-none})"

if [[ " $LABELS " == *" size-exempt "* ]]; then
    echo "size-exempt label set, skipping enforcement"
    exit 0
fi

if (( added > budget )); then
    echo "over budget: split this layer, or label it mechanical (verbatim moves, renames, deletes only)"
    echo "size-exempt requires the reviewer to add the label"
    exit 1
fi
