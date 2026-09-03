#!/usr/bin/env bash
# Fail when the agent files drift from the layout AGENTS.md describes
set -euo pipefail

cd "$(dirname "$0")/../.."

status=0
check_link() {
    local path="$1"
    local target="$2"
    if [[ -L "$path" && "$(readlink "$path")" == "$target" ]]; then
        return
    fi
    echo "$path must link to $target"
    status=1
}

check_references() {
    while IFS= read -r document; do
        while IFS= read -r reference; do
            if [[ "$reference" == http* || "$reference" == \#* ]]; then
                continue
            fi
            reference=${reference%%#*}
            path="$(dirname "$document")/$reference"
            if [[ -n "$reference" && ! -e "$path" ]]; then
                echo "$document references missing $reference"
                status=1
            fi
        done < <(grep -oP '\]\(\K[^)]+(?=\))' "$document" || true)
    done < <(find .agents/skills -name SKILL.md -type f -print)
}

check_link CLAUDE.md AGENTS.md
check_link .claude/skills ../.agents/skills

for skill in .agents/skills/*/SKILL.md; do
    directory=$(basename "$(dirname "$skill")")
    name=$(sed -n 's/^name: //p' "$skill" | head -n 1)
    if [[ "$directory" != "$name" ]]; then
        echo "$skill declares the name $name"
        status=1
    fi
done

check_references
exit "$status"
