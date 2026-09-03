# sngram justfile
# Usage: just <command> [args]

set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

# List every command
default:
    @just --list

# Scan docs and layout, then format-check, type-check and lint the workspace and both sngram feature sets
check:
    bash .github/scripts/check-agent-layout.sh
    bash scripts/doc-scan.sh
    bash scripts/layout-scan.sh
    cargo fmt --all -- --check
    cargo check --workspace --all-targets --all-features
    cargo check -p sngram --all-targets --no-default-features
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo clippy -p sngram --all-targets --no-default-features -- -D warnings

# Count the comments that still run past one line
comment-scan:
    bash scripts/comment-scan.sh

# Format the workspace
fmt:
    cargo fmt --all

# Build every target
build:
    cargo build --workspace --all-targets

# Run the test suite, elgrep last because its daemon tests lose their 30 ms vouch under load
test:
    cargo nextest run --workspace --exclude elgrep
    cargo nextest run -p sngram --all-features
    cargo nextest run -p sngram --no-default-features
    cargo nextest run -p elgrep

# Run the test suite the way the gate does
test-ci:
    cargo nextest run --profile ci --workspace --exclude elgrep
    cargo nextest run --profile ci -p sngram --all-features
    cargo nextest run --profile ci -p sngram --no-default-features
    cargo nextest run --profile ci -p elgrep

# Run every doc example, which nextest cannot
test-doc:
    cargo test -p sngram --doc

# Build the docs the way docs.rs does, failing on any warning or broken link
doc-check:
    RUSTDOCFLAGS="-D warnings" cargo doc -p sngram --all-features --no-deps

# Build the docs and open them
docs-open:
    RUSTDOCFLAGS="-D warnings" cargo doc -p sngram --all-features --no-deps --open

# Type-check on the minimum supported Rust version the manifest declares
msrv:
    cargo +1.96 check --workspace --all-targets --locked

# Report every sngram API break against the last release, or against the given revision
# --release-type minor asks whether a minor release would be legal, so a major bump in the
# manifest cannot mask the breakage the way a bare run does
semver-check baseline="":
    cargo semver-checks -p sngram --all-features --release-type minor {{ if baseline != "" { "--baseline-rev " + baseline } else { "" } }}

# Build the crates.io packages and list what ships in them
package-check:
    cargo package -p sngram -p elgrep --locked --allow-dirty
    cargo package -p sngram -p elgrep --locked --allow-dirty --list

# Check licenses, advisories, duplicate versions and sources
audit:
    cargo deny check

# Build the library benches with one CodSpeed instrument; the repo-local native codegen cannot run under valgrind
bench-build mode="simulation":
    RUSTFLAGS="" cargo codspeed build -m {{mode}} -p sngram-benches

# Run the built library benches
bench-run mode="simulation":
    RUSTFLAGS="" cargo codspeed run -m {{mode}} -p sngram-benches

# Build and run the library benches the way CodSpeed does
bench:
    just bench-build
    just bench-run

# Lint the python package and the trainer with ruff
py-lint:
    cd crates/python && uvx ruff@0.16.0 check --no-cache .
    cd train && uvx ruff@0.16.0 check --no-cache .

# Build the python extension and run the python and trainer tests
py-test:
    cd crates/python && uv sync && uv run pytest -q
    cd train && uv sync && uv run pytest -q

# Run the python benches the way CodSpeed does
py-bench:
    cd crates/python && RUSTFLAGS="" uv sync && uv run --no-sync pytest benchmarks --codspeed

# Build the wheel into a clean directory and import it into a fresh environment
wheel:
    rm -rf target/wheel-smoke
    cd crates/python && uvx maturin build --release --out ../../target/wheel-smoke
    bash scripts/wheel-smoke.sh

# Enforce the reviewable diff budget, defaulting to this branch against main
size:
    BASE_SHA="${BASE_SHA:-$(git merge-base origin/main HEAD)}" \
    HEAD_SHA="${HEAD_SHA:-HEAD}" \
    bash scripts/pr-size-gate.sh

# Install the git hooks
hooks:
    prek install --hook-type pre-commit --hook-type pre-push

# Run the hooks against every file
hooks-run:
    prek run --all-files

# Print the version the next merge to main would release
release-plan:
    bash scripts/release.sh --dry-run

# Run every check the gate blocks on
ci:
    just check
    just test-ci
    just test-doc
    just doc-check
    just package-check
    just audit
    just msrv
    just py-lint
    just py-test
    just wheel
    just bench
    just py-bench
    rm -rf target/tmp

# Remove every build artifact
clean:
    cargo clean

# Run the fp suite against a corpus directory with the release eg
suite corpus=".":
    cd {{corpus}} && {{justfile_directory()}}/target/release/eg --bench

# Run the fp suite against the guard corpus
guard corpus="~/repos/django":
    just suite {{corpus}}

# Build, check, test, lint, bench, release or run elgrep alone
eg action="help" *args:
    case "{{action}}" in \
      build) cargo build -p elgrep {{args}} ;; \
      check) cargo check -p elgrep {{args}} ;; \
      test) cargo nextest run -p elgrep {{args}} ;; \
      clippy) cargo clippy -p elgrep --all-targets -- -D warnings {{args}} ;; \
      bench) cargo bench -p elgrep --bench index {{args}} ;; \
      release) RUSTFLAGS="-C target-cpu=native" cargo build -p elgrep --release {{args}} ;; \
      run) cargo run -p elgrep -- {{args}} ;; \
      *) printf '%s\n' 'usage: just eg build|check|test|clippy|bench|release|run [args...]'; exit 2 ;; \
    esac
