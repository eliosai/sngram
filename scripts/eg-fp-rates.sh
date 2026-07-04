#!/usr/bin/env bash
set -u

usage() {
  cat <<'EOF'
usage: scripts/eg-fp-rates.sh CORPUS_ROOT [OUT_TSV]

Runs eg indexed searches, compares prefilter candidates with exact matching
files, and writes a TSV table of false-positive metrics.

Environment:
  EG_BIN       eg binary to run (default: target/release/eg)
  QUERIES     TSV file: label<TAB>pattern<TAB>flags (optional)
  EXTRA_ARGS  extra eg args inserted before the pattern (optional)

Output columns:
  label, pattern, flags, total_files, postings, candidates, actual, fp,
  fp_per_candidate_pct, fpr_nonmatch_pct, precision_pct, lookup_ms,
  filter_ms, verify_ms, total_ms, plan
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" || $# -lt 1 ]]; then
  usage
  exit 0
fi

EG_BIN=${EG_BIN:-target/release/eg}
ROOT=$1
OUT=${2:-/tmp/eg-fp-rates.tsv}
QUERY_FILE=${QUERIES:-}
WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/eg-fp-rates.XXXXXX")
trap 'rm -rf "$WORKDIR"' EXIT

if [[ ! -x "$EG_BIN" ]]; then
  echo "eg-fp-rates: EG_BIN is not executable: $EG_BIN" >&2
  exit 2
fi
if [[ ! -d "$ROOT" ]]; then
  echo "eg-fp-rates: corpus root is not a directory: $ROOT" >&2
  exit 2
fi

ms() {
  awk -v v="$1" 'BEGIN {
    if (v ~ /ns$/) { sub(/ns$/, "", v); printf "%.3f", v / 1000000 }
    else if (v ~ /µs$/) { sub(/µs$/, "", v); printf "%.3f", v / 1000 }
    else if (v ~ /ms$/) { sub(/ms$/, "", v); printf "%.3f", v }
    else if (v ~ /s$/) { sub(/s$/, "", v); printf "%.3f", v * 1000 }
    else if (v == "") { printf "0.000" }
    else { printf "%.3f", v }
  }'
}

pct() {
  awk -v n="$1" -v d="$2" 'BEGIN {
    if (d <= 0) printf "0.00"; else printf "%.2f", n * 100 / d
  }'
}

default_queries() {
  cat <<'EOF'
everything	Everything
sched_clock	sched_clock
sched_class	sched[_-]clock
sched_alt	sched_clock|sched-clock
sched_gap	sched.*clock
max_file_size_i	max_file_size	-i
max_min_file_size	(max|min)_file_size
max_word_size	max_\w+_size
request_irq	request_irq
request_free_irq	(request_irq|free_irq)
kfree	kfree
config_preempt	CONFIG_PREEMPT
EOF
}

query_rows() {
  if [[ -n "$QUERY_FILE" ]]; then
    grep -v '^[[:space:]]*$' "$QUERY_FILE" | grep -v '^[[:space:]]*#'
  else
    default_queries
  fi
}

printf 'label\tpattern\tflags\ttotal_files\tpostings\tcandidates\tactual\tfp\tfp_per_candidate_pct\tfpr_nonmatch_pct\tprecision_pct\tlookup_ms\tfilter_ms\tverify_ms\ttotal_ms\tplan\n' >"$OUT"

while IFS=$'\t' read -r label pattern flags; do
  [[ -n "${label:-}" ]] || continue
  # A row with an empty pattern column carries its pattern in flags (e.g. -e).
  [[ -n "${pattern:-}" || -n "${flags:-}" ]] || continue
  safe_label=$(printf '%s' "$label" | tr -c 'A-Za-z0-9_.-' '_')
  stdout="$WORKDIR/$safe_label.out"
  stderr="$WORKDIR/$safe_label.err"

  if [[ -n "${pattern:-}" ]]; then query=("$pattern" "$ROOT"); else query=("$ROOT"); fi

  # shellcheck disable=SC2086
  "$EG_BIN" --debug --files-with-matches ${EXTRA_ARGS:-} ${flags:-} -- "${query[@]}" >"$stdout" 2>"$stderr"
  status=$?
  if [[ $status -ne 0 && $status -ne 1 ]]; then
    if grep -q -- '--no-index' "$stderr"; then
      printf '%s\t%s\t%s\t0\t0\t0\t0\t0\t0.00\t0.00\t0.00\t0.000\t0.000\t0.000\t0.000\tunsupported\n' \
        "$label" "$pattern" "${flags:-}" >>"$OUT"
      continue
    fi
    echo "eg-fp-rates: query failed ($status): $label" >&2
    sed -n '1,80p' "$stderr" >&2
    exit "$status"
  fi

  total_files=$(grep -o 'f\(ast freshness snapshot\|reshness manifest\) for [0-9]* files' "$stderr" | tail -n1 | grep -o '[0-9]*')
  if [[ -z "${total_files:-}" ]]; then
    total_files=$(grep -o 'collected [0-9]* haystacks' "$stderr" | tail -n1 | grep -o '[0-9]*')
  fi
  postings=$(grep -o 'postings candidates=[0-9]*' "$stderr" | tail -n1 | sed 's/.*=//')
  retained=$(grep -o 'literal filter candidates=[0-9]* retained=[0-9]*' "$stderr" | tail -n1 | sed 's/.*retained=//')
  actual=$(wc -l <"$stdout" | tr -d ' ')
  total_files=${total_files:-0}
  postings=${postings:-0}
  candidates=${retained:-$postings}
  if grep -q 'falling back to unindexed scan' "$stderr"; then
    lookup_raw=$(grep -o 'lookup_time=[^ ]*' "$stderr" | tail -n1 | sed 's/lookup_time=//')
    total_raw=$(grep -o 'total_query_time=[^ ]*' "$stderr" | tail -n1 | sed 's/total_query_time=//')
    printf '%s\t%s\t%s\t%s\t0\t0\t%s\t0\t0.00\t0.00\t0.00\t%s\t0.000\t0.000\t%s\tfallback\n' \
      "$label" "$pattern" "${flags:-}" "$total_files" "$actual" \
      "$(ms "$lookup_raw")" "$(ms "$total_raw")" >>"$OUT"
    continue
  fi
  fp=$((candidates - actual))
  nonmatch=$((total_files - actual))
  precision=$(pct "$actual" "$candidates")
  fp_per_candidate=$(pct "$fp" "$candidates")
  fpr_nonmatch=$(pct "$fp" "$nonmatch")

  lookup_raw=$(grep -o 'lookup_time=[^ ]*' "$stderr" | tail -n1 | sed 's/lookup_time=//')
  filter_raw=$(grep -o 'filter_time=[^ ]*' "$stderr" | tail -n1 | sed 's/filter_time=//')
  verify_raw=$(grep -o 'verify_time=[^ ]*' "$stderr" | tail -n1 | sed 's/verify_time=//')
  total_raw=$(grep 'verified' "$stderr" | tail -n1 | grep -o 'total_time=[^ ]*' | sed 's/total_time=//')
  if [[ -z "${total_raw:-}" ]]; then
    total_raw=$(grep -o 'total_query_time=[^ ]*' "$stderr" | tail -n1 | sed 's/total_query_time=//')
  fi
  plan=$(grep 'query plan:' "$stderr" | tail -n1 | sed 's/.*query plan: //')

  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$label" "$pattern" "${flags:-}" "$total_files" "$postings" "$candidates" \
    "$actual" "$fp" "$fp_per_candidate" "$fpr_nonmatch" "$precision" \
    "$(ms "$lookup_raw")" "$(ms "$filter_raw")" "$(ms "$verify_raw")" \
    "$(ms "$total_raw")" "$plan" >>"$OUT"
done < <(query_rows)

cat "$OUT"
