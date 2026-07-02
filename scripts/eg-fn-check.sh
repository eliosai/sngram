#!/usr/bin/env bash
set -u

usage() {
  cat <<'EOF'
usage: scripts/eg-fn-check.sh CORPUS_ROOT

Differential false-negative check: for every query in the corpus, indexed
search results must equal --no-index full-scan results. A file the full
scan matches that the indexed search misses is a soundness violation.

Environment:
  EG_BIN   eg binary to run (default: target/release/eg)
  QUERIES  TSV file: label<TAB>pattern<TAB>flags (default: scripts/fp-queries.tsv)
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" || $# -lt 1 ]]; then
  usage
  exit 0
fi

EG_BIN=${EG_BIN:-target/release/eg}
ROOT=$1
QUERY_FILE=${QUERIES:-scripts/fp-queries.tsv}
WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/eg-fn-check.XXXXXX")
trap 'rm -rf "$WORKDIR"' EXIT

fail=0
total=0
skipped=0

while IFS=$'\t' read -r label pattern flags; do
  [[ -n "${label:-}" && -n "${pattern:-}" ]] || continue
  case $label in \#*) continue ;; esac

  # shellcheck disable=SC2086
  "$EG_BIN" --files-with-matches ${flags:-} -- "$pattern" "$ROOT" >"$WORKDIR/indexed.raw" 2>"$WORKDIR/err"
  status=$?
  sort "$WORKDIR/indexed.raw" >"$WORKDIR/indexed"
  if [[ $status -ne 0 && $status -ne 1 ]]; then
    if grep -q 'use --no-index' "$WORKDIR/err"; then
      skipped=$((skipped + 1))
      continue
    fi
    echo "eg-fn-check: $label failed ($status)" >&2
    sed -n '1,5p' "$WORKDIR/err" >&2
    exit "$status"
  fi

  # shellcheck disable=SC2086
  "$EG_BIN" --no-index --files-with-matches ${flags:-} -- "$pattern" "$ROOT" 2>/dev/null | sort >"$WORKDIR/scan"

  total=$((total + 1))
  missed=$(comm -13 "$WORKDIR/indexed" "$WORKDIR/scan")
  if [[ -n "$missed" ]]; then
    fail=$((fail + 1))
    echo "FALSE NEGATIVE: $label ($pattern ${flags:-}) missed:"
    echo "$missed" | head -5 | sed 's/^/  /'
  fi
done < <(grep -v '^[[:space:]]*$' "$QUERY_FILE" | grep -v '^[[:space:]]*#')

echo "eg-fn-check: $total compared, $skipped unsupported-skipped, $fail with false negatives"
[[ $fail -eq 0 ]]
