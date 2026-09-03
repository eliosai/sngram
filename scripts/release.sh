#!/usr/bin/env bash
# Release from main: pick the version, write the changelog, tag, publish sngram then elgrep, cut the release
# `--dry-run` prints the version it would release and stops
set -euo pipefail

cd "$(dirname "$0")/.."

dry_run=false
if [[ "${1:-}" == "--dry-run" ]]; then
    dry_run=true
fi

current=$(cargo metadata --no-deps --format-version 1 |
    python3 -c 'import json, sys; meta = json.load(sys.stdin); print(next(p["version"] for p in meta["packages"] if p["name"] == "sngram"))')
last_tag=$(git describe --tags --abbrev=0 --match 'v*' 2>/dev/null || true)

published() {
    curl -sf "https://index.crates.io/$2" | grep -q "\"vers\":\"$1\""
}

# major when the API breaks or a subject carries `!`, minor for a feat, patch for a fix, perf or refactor
bump_kind() {
    local subjects
    subjects=$(git log --format=%s "$last_tag..HEAD")
    if grep -qE '^[a-z]+(\([^)]*\))?!:' <<<"$subjects"; then
        echo major
    elif ! cargo semver-checks -p sngram --all-features --baseline-rev "$last_tag" >/dev/null 2>&1; then
        echo major
    elif grep -qE '^feat(\([^)]*\))?:' <<<"$subjects"; then
        echo minor
    elif grep -qE '^(fix|perf|refactor)(\([^)]*\))?:' <<<"$subjects"; then
        echo patch
    else
        echo none
    fi
}

# a major bump before 1.0 moves the minor, the way cargo reads 0.x
next_version() {
    local major minor patch
    IFS=. read -r major minor patch <<<"${last_tag#v}"
    case "$1" in
        major) if ((major == 0)); then echo "0.$((minor + 1)).0"; else echo "$((major + 1)).0.0"; fi ;;
        minor) if ((major == 0)); then echo "0.$((minor + 1)).0"; else echo "$major.$((minor + 1)).0"; fi ;;
        patch) echo "$major.$minor.$((patch + 1))" ;;
    esac
}

newer() {
    python3 -c 'import sys; a, b = (tuple(map(int, v.split("."))) for v in sys.argv[1:]); sys.exit(0 if a > b else 1)' "$1" "$2"
}

if [[ -z "$last_tag" ]]; then
    next=$current
    reason="first tagged release, from Cargo.toml"
elif [[ "v$current" == "$last_tag" ]] && ! published "$current" sn/gr/sngram; then
    next=$current
    reason="tagged but not on crates.io"
elif [[ "v$current" != "$last_tag" ]]; then
    if ! newer "$current" "${last_tag#v}"; then
        echo "Cargo.toml says $current but $last_tag is already released" >&2
        exit 1
    fi
    next=$current
    reason="version set by hand"
else
    kind=$(bump_kind)
    if [[ "$kind" == none ]]; then
        echo "nothing to release since $last_tag"
        exit 0
    fi
    next=$(next_version "$kind")
    reason="$kind bump from $last_tag"
fi

echo "release v$next ($reason)"
if $dry_run; then
    exit 0
fi

if [[ "$next" != "$current" ]]; then
    sed -i "s/^version = \"$current\"$/version = \"$next\"/" Cargo.toml
    sed -i "s/^sngram = { version = \"$current\", path = \"crates\/lib\" }$/sngram = { version = \"$next\", path = \"crates\/lib\" }/" Cargo.toml
    sed -i "s/^version = \"$current\"$/version = \"$next\"/" crates/python/pyproject.toml
    sed -i "s/^__version__ = \"$current\"$/__version__ = \"$next\"/" crates/python/sngram/__init__.py
    cargo update --workspace
    (cd crates/python && uv lock -q)
    (cd train && uv lock -q)
fi

notes=$(mktemp)
git-cliff --tag "v$next" --unreleased --strip all >"$notes"
git-cliff --tag "v$next" --unreleased --prepend CHANGELOG.md

git config user.name "github-actions[bot]"
git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
git add Cargo.toml Cargo.lock CHANGELOG.md crates/python/pyproject.toml crates/python/sngram/__init__.py crates/python/uv.lock train/uv.lock
if ! git diff --cached --quiet; then
    git commit -m "chore(release): v$next"
fi
if ! git rev-parse -q --verify "refs/tags/v$next" >/dev/null; then
    git tag -a "v$next" -m "v$next"
fi
git push origin HEAD:main
git push origin "v$next"

if ! published "$next" sn/gr/sngram; then
    cargo publish -p sngram --locked
fi
if ! published "$next" el/gr/elgrep; then
    cargo publish -p elgrep --locked
fi
if ! gh release view "v$next" >/dev/null 2>&1; then
    gh release create "v$next" --title "v$next" --notes-file "$notes"
fi
