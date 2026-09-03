#!/usr/bin/env bash
# Fail on scoped visibility or a module file paired with a directory
set -euo pipefail

cd "$(dirname "$0")/.."

status=0
if grep -rnE 'pub\((crate|super|self|in )' crates --include='*.rs'; then
    echo "scoped visibility is forbidden" >&2
    status=1
fi

while IFS= read -r file; do
    stem="${file%.rs}"
    if [[ -d "$stem" ]]; then
        echo "$file pairs with $stem/, use $stem/mod.rs" >&2
        status=1
    fi
done < <(find crates -name '*.rs' -not -path '*/target/*' -not -name mod.rs -not -name lib.rs -not -name main.rs)

exit "$status"
